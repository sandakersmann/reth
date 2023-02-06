use crate::{
    db::Transaction, exec_or_return, ExecAction, ExecInput, ExecOutput, Stage, StageError, StageId,
    UnwindInput, UnwindOutput,
};
use itertools::Itertools;
use rayon::prelude::*;
use reth_db::{
    cursor::{DbCursorRO, DbCursorRW},
    database::Database,
    tables,
    transaction::{DbTx, DbTxMut},
    Error as DbError,
};
use reth_primitives::TxNumber;
use std::fmt::Debug;
use thiserror::Error;
use tracing::*;

const SENDER_RECOVERY: StageId = StageId("SenderRecovery");

/// The sender recovery stage iterates over existing transactions,
/// recovers the transaction signer and stores them
/// in [`TxSenders`][reth_db::tables::TxSenders] table.
#[derive(Clone, Debug)]
pub struct SenderRecoveryStage {
    /// The size of the chunk for parallel sender recovery
    pub batch_size: usize,
    /// The size of inserted items after which the control
    /// flow will be returned to the pipeline for commit
    pub commit_threshold: u64,
}

impl Default for SenderRecoveryStage {
    fn default() -> Self {
        Self { batch_size: 250000, commit_threshold: 10000 }
    }
}

// TODO(onbjerg): Should unwind
#[derive(Error, Debug)]
enum SenderRecoveryStageError {
    #[error("Sender recovery failed for transaction {tx}.")]
    SenderRecovery { tx: TxNumber },
}

impl From<SenderRecoveryStageError> for StageError {
    fn from(error: SenderRecoveryStageError) -> Self {
        StageError::Fatal(Box::new(error))
    }
}

#[async_trait::async_trait]
impl<DB: Database> Stage<DB> for SenderRecoveryStage {
    /// Return the id of the stage
    fn id(&self) -> StageId {
        SENDER_RECOVERY
    }

    /// Retrieve the range of transactions to iterate over by querying
    /// [`BlockBodies`][reth_db::tables::BlockBodies],
    /// collect transactions within that range,
    /// recover signer for each transaction and store entries in
    /// the [`TxSenders`][reth_db::tables::TxSenders] table.
    async fn execute(
        &mut self,
        tx: &mut Transaction<'_, DB>,
        input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        let ((start_block, end_block), capped) =
            exec_or_return!(input, self.commit_threshold, "sync::stages::sender_recovery");

        // Look up the start index for the transaction range
        let start_tx_index = tx.get_block_body_by_num(start_block)?.start_tx_id;

        // Look up the end index for transaction range (inclusive)
        let end_tx_index = tx.get_block_body_by_num(end_block)?.last_tx_index();

        // No transactions to walk over
        if start_tx_index > end_tx_index {
            info!(target: "sync::stages::sender_recovery", start_tx_index, end_tx_index, "Target transaction already reached");
            return Ok(ExecOutput { stage_progress: end_block, done: true })
        }

        // Acquire the cursor for inserting elements
        let mut senders_cursor = tx.cursor_write::<tables::TxSenders>()?;

        // Acquire the cursor over the transactions
        let mut tx_cursor = tx.cursor_read::<tables::Transactions>()?;
        // Walk the transactions from start to end index (inclusive)
        let entries = tx_cursor.walk_range(start_tx_index..end_tx_index + 1)?;

        // Iterate over transactions in chunks
        info!(target: "sync::stages::sender_recovery", start_tx_index, end_tx_index, "Recovering senders");
        for chunk in &entries.chunks(self.batch_size) {
            let transactions = chunk.collect::<Result<Vec<_>, DbError>>()?;
            // Recover signers for the chunk in parallel
            let recovered = transactions
                .into_par_iter()
                .map(|(tx_id, transaction)| {
                    trace!(target: "sync::stages::sender_recovery", tx_id, hash = ?transaction.hash(), "Recovering sender");
                    let signer =
                        transaction.recover_signer().ok_or_else::<StageError, _>(|| {
                            SenderRecoveryStageError::SenderRecovery { tx: tx_id }.into()
                        })?;
                    Ok((tx_id, signer))
                })
                .collect::<Result<Vec<_>, StageError>>()?;
            // Append the signers to the table
            recovered.into_iter().try_for_each(|(id, sender)| senders_cursor.append(id, sender))?;
        }

        let done = !capped;
        info!(target: "sync::stages::sender_recovery", stage_progress = end_block, done, "Sync iteration finished");
        Ok(ExecOutput { stage_progress: end_block, done })
    }

    /// Unwind the stage.
    async fn unwind(
        &mut self,
        tx: &mut Transaction<'_, DB>,
        input: UnwindInput,
    ) -> Result<UnwindOutput, StageError> {
        info!(target: "sync::stages::sender_recovery", to_block = input.unwind_to, "Unwinding");
        // Lookup latest tx id that we should unwind to
        let latest_tx_id = tx.get_block_body_by_num(input.unwind_to)?.last_tx_index();
        tx.unwind_table_by_num::<tables::TxSenders>(latest_tx_id)?;
        Ok(UnwindOutput { stage_progress: input.unwind_to })
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use reth_interfaces::test_utils::generators::{random_block, random_block_range};
    use reth_primitives::{BlockNumber, SealedBlock, H256};

    use super::*;
    use crate::test_utils::{
        stage_test_suite_ext, ExecuteStageTestRunner, StageTestRunner, TestRunnerError,
        TestTransaction, UnwindStageTestRunner, PREV_STAGE_ID,
    };

    stage_test_suite_ext!(SenderRecoveryTestRunner, sender_recovery);

    /// Execute a block range with a single transaction
    #[tokio::test]
    async fn execute_single_transaction() {
        let (previous_stage, stage_progress) = (500, 100);

        // Set up the runner
        let runner = SenderRecoveryTestRunner::default();
        let input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(stage_progress),
        };

        // Insert blocks with a single transaction at block `stage_progress + 10`
        let non_empty_block_number = stage_progress + 10;
        let blocks = (stage_progress..input.previous_stage_progress() + 1)
            .map(|number| {
                random_block(number, None, Some((number == non_empty_block_number) as u8), None)
            })
            .collect::<Vec<_>>();
        runner.tx.insert_blocks(blocks.iter(), None).expect("failed to insert blocks");

        let rx = runner.execute(input);

        // Assert the successful result
        let result = rx.await.unwrap();
        assert_matches!(
            result,
            Ok(ExecOutput { done, stage_progress })
                if done && stage_progress == previous_stage
        );

        // Validate the stage execution
        assert!(runner.validate_execution(input, result.ok()).is_ok(), "execution validation");
    }

    /// Execute the stage twice with input range that exceeds the commit threshold
    #[tokio::test]
    async fn execute_intermediate_commit() {
        let threshold = 50;
        let mut runner = SenderRecoveryTestRunner::default();
        runner.set_threshold(threshold);
        let (stage_progress, previous_stage) = (1000, 1100); // input exceeds threshold
        let first_input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(stage_progress),
        };

        // Seed only once with full input range
        runner.seed_execution(first_input).expect("failed to seed execution");

        // Execute first time
        let result = runner.execute(first_input).await.unwrap();
        let expected_progress = stage_progress + threshold;
        assert_matches!(
            result,
            Ok(ExecOutput { done: false, stage_progress })
                if stage_progress == expected_progress
        );

        // Execute second time
        let second_input = ExecInput {
            previous_stage: Some((PREV_STAGE_ID, previous_stage)),
            stage_progress: Some(expected_progress),
        };
        let result = runner.execute(second_input).await.unwrap();
        assert_matches!(
            result,
            Ok(ExecOutput { done: true, stage_progress })
                if stage_progress == previous_stage
        );

        assert!(runner.validate_execution(first_input, result.ok()).is_ok(), "validation failed");
    }

    struct SenderRecoveryTestRunner {
        tx: TestTransaction,
        threshold: u64,
    }

    impl Default for SenderRecoveryTestRunner {
        fn default() -> Self {
            Self { threshold: 1000, tx: TestTransaction::default() }
        }
    }

    impl SenderRecoveryTestRunner {
        fn set_threshold(&mut self, threshold: u64) {
            self.threshold = threshold;
        }

        /// # Panics
        ///
        /// 1. If there are any entries in the [tables::TxSenders] table above
        ///    a given block number.
        ///
        /// 2. If the is no requested block entry in the bodies table,
        ///    but [tables::TxSenders] is not empty.
        fn ensure_no_senders_by_block(&self, block: BlockNumber) -> Result<(), TestRunnerError> {
            let body_result = self.tx.inner().get_block_body_by_num(block);
            match body_result {
                Ok(body) => self
                    .tx
                    .ensure_no_entry_above::<tables::TxSenders, _>(body.last_tx_index(), |key| {
                        key
                    })?,
                Err(_) => {
                    assert!(self.tx.table_is_empty::<tables::TxSenders>()?);
                }
            };

            Ok(())
        }
    }

    impl StageTestRunner for SenderRecoveryTestRunner {
        type S = SenderRecoveryStage;

        fn tx(&self) -> &TestTransaction {
            &self.tx
        }

        fn stage(&self) -> Self::S {
            SenderRecoveryStage { batch_size: 100, commit_threshold: self.threshold }
        }
    }

    impl ExecuteStageTestRunner for SenderRecoveryTestRunner {
        type Seed = Vec<SealedBlock>;

        fn seed_execution(&mut self, input: ExecInput) -> Result<Self::Seed, TestRunnerError> {
            let stage_progress = input.stage_progress.unwrap_or_default();
            let end = input.previous_stage_progress() + 1;

            let blocks = random_block_range(stage_progress..end, H256::zero(), 0..2);
            self.tx.insert_blocks(blocks.iter(), None)?;
            Ok(blocks)
        }

        fn validate_execution(
            &self,
            input: ExecInput,
            output: Option<ExecOutput>,
        ) -> Result<(), TestRunnerError> {
            match output {
                Some(output) => self.tx.query(|tx| {
                    let start_block = input.stage_progress.unwrap_or_default() + 1;
                    let end_block = output.stage_progress;

                    if start_block > end_block {
                        return Ok(())
                    }

                    let start_hash = tx.get::<tables::CanonicalHeaders>(start_block)?.unwrap();
                    let mut body_cursor = tx.cursor_read::<tables::BlockBodies>()?;
                    body_cursor.seek_exact((start_block, start_hash).into())?;

                    while let Some((_, body)) = body_cursor.next()? {
                        for tx_id in body.tx_id_range() {
                            let transaction = tx
                                .get::<tables::Transactions>(tx_id)?
                                .expect("no transaction entry");
                            let signer =
                                transaction.recover_signer().expect("failed to recover signer");
                            assert_eq!(Some(signer), tx.get::<tables::TxSenders>(tx_id)?);
                        }
                    }

                    Ok(())
                })?,
                None => {
                    self.ensure_no_senders_by_block(input.stage_progress.unwrap_or_default())?
                }
            };

            Ok(())
        }
    }

    impl UnwindStageTestRunner for SenderRecoveryTestRunner {
        fn validate_unwind(&self, input: UnwindInput) -> Result<(), TestRunnerError> {
            self.ensure_no_senders_by_block(input.unwind_to)
        }
    }
}
