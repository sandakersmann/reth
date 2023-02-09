use crate::{
    dirs::{ConfigPath, DbPath, PlatformPath},
    node::{handle_events, NodeEvent},
    prometheus_exporter,
    utils::{chainspec::genesis_value_parser, init::init_db, parse_socket_address},
    NetworkOpts,
};
use clap::{crate_version, Parser};
use eyre::Context;
use fdlimit::raise_fd_limit;
use futures::{stream::select as stream_select, Stream, StreamExt};
use reth_consensus::beacon::BeaconConsensus;
use reth_db::mdbx::{Env, WriteMap};
use reth_downloaders::{
    bodies, bodies::bodies::BodiesDownloaderBuilder, headers,
    headers::reverse_headers::ReverseHeadersDownloaderBuilder, test_utils::FileClient,
};
use reth_interfaces::{
    consensus::{Consensus, ForkchoiceState},
    p2p::{
        bodies::{client::BodiesClient, downloader::BodyDownloader},
        headers::{client::HeadersClient, downloader::HeaderDownloader},
    },
    sync::SyncStateUpdater,
};
use reth_net_nat::NatResolver;
use reth_network::{NetworkConfig, NetworkEvent};
use reth_network_api::NetworkInfo;
use reth_primitives::{BlockNumber, ChainSpec, H256};
use reth_provider::ShareableDatabase;
use reth_rpc_builder::{RethRpcModule, RpcServerConfig, TransportRpcModuleConfig};
use reth_staged_sync::{utils::init::init_genesis, Config};
use reth_stages::{
    prelude::*,
    stages::{ExecutionStage, SenderRecoveryStage, TotalDifficultyStage},
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::select;
use tracing::{debug, info, warn};

/// Syncs RLP encoded blocks from a file.
#[derive(Debug, Parser)]
pub struct ImportCommand {
    /// The path to the configuration file to use.
    #[arg(long, value_name = "FILE", verbatim_doc_comment, default_value_t)]
    config: PlatformPath<ConfigPath>,

    /// The path to the database folder.
    ///
    /// Defaults to the OS-specific data directory:
    ///
    /// - Linux: `$XDG_DATA_HOME/reth/db` or `$HOME/.local/share/reth/db`
    /// - Windows: `{FOLDERID_RoamingAppData}/reth/db`
    /// - macOS: `$HOME/Library/Application Support/reth/db`
    #[arg(long, value_name = "PATH", verbatim_doc_comment, default_value_t)]
    db: PlatformPath<DbPath>,

    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain or the path to a chain specification file.
    ///
    /// Built-in chains:
    /// - mainnet
    /// - goerli
    /// - sepolia
    #[arg(
        long,
        value_name = "CHAIN_OR_PATH",
        verbatim_doc_comment,
        default_value = "mainnet",
        value_parser = genesis_value_parser
    )]
    chain: ChainSpec,

    /// The path to a block file for import.
    /// When specified, this syncs RLP encoded blocks from a file.
    ///
    /// The online stages (headers and bodies) are replaced by a file import, after which the
    /// remaining stages are executed.
    #[arg(long, value_name = "IMPORT_PATH", verbatim_doc_comment)]
    blocks: PlatformPath<ConfigPath>,
}

impl ImportCommand {
    /// Execute `import` command
    pub async fn execute(mut self) -> eyre::Result<()> {
        info!(target: "reth::cli", "reth {} starting", crate_version!());

        // Raise the fd limit of the process.
        // Does not do anything on windows.
        raise_fd_limit();

        let mut config: Config = self.load_config()?;
        info!(target: "reth::cli", path = %self.db, "Configuration loaded");

        info!(target: "reth::cli", path = %self.db, "Opening database");
        let db = Arc::new(init_db(&self.db)?);
        info!(target: "reth::cli", "Database opened");

        debug!(target: "reth::cli", chainspec=?self.chain, "Initializing genesis");
        init_genesis(db.clone(), self.chain.clone())?;

        // create a new FileClient
        info!(target: "reth::cli", "Importing chain file");
        let file_client = Arc::new(FileClient::new(&self.blocks).await?);

        // override the tip
        let tip = file_client.tip().expect("file client has no tip");
        info!(target: "reth::cli", "Chain file imported");

        let (consensus, notifier) = BeaconConsensus::builder().build(self.chain.clone());
        debug!(target: "reth::cli", %tip, "Tip manually set");
        notifier.send(ForkchoiceState {
            head_block_hash: tip,
            safe_block_hash: tip,
            finalized_block_hash: tip,
        })?;
        info!(target: "reth::cli", "Consensus engine initialized");

        let (mut pipeline, events) =
            self.build_import_pipeline(config, db.clone(), &consensus, file_client).await?;

        tokio::spawn(handle_events(events));

        // Run pipeline
        info!(target: "reth::cli", "Starting sync pipeline");
        pipeline.run(db.clone()).await?;

        Ok(())
    }

    async fn build_import_pipeline<C>(
        &self,
        config: Config,
        db: Arc<Env<WriteMap>>,
        consensus: &Arc<C>,
        file_client: Arc<FileClient>,
    ) -> eyre::Result<(Pipeline<Env<WriteMap>, impl SyncStateUpdater>, impl Stream<Item = NodeEvent>)>
    where
        C: Consensus + 'static,
    {
        let header_downloader = ReverseHeadersDownloaderBuilder::default()
            .request_limit(config.stages.headers.downloader_batch_size)
            .stream_batch_size(config.stages.headers.commit_threshold as usize)
            .build(consensus.clone(), file_client.clone())
            .as_task();

        let body_downloader = BodiesDownloaderBuilder::default()
            .with_stream_batch_size(config.stages.bodies.downloader_stream_batch_size)
            .with_request_limit(config.stages.bodies.downloader_request_limit)
            .with_max_buffered_responses(config.stages.bodies.downloader_max_buffered_responses)
            .with_concurrent_requests_range(
                config.stages.bodies.downloader_min_concurrent_requests..=
                    config.stages.bodies.downloader_max_concurrent_requests,
            )
            .build(file_client.clone(), consensus.clone(), db.clone())
            .as_task();

        let mut pipeline = Pipeline::builder()
            .with_sync_state_updater(file_client.clone())
            .add_stages(
                OnlineStages::new(consensus.clone(), header_downloader, body_downloader).set(
                    TotalDifficultyStage {
                        chain_spec: self.chain.clone(),
                        commit_threshold: config.stages.total_difficulty.commit_threshold,
                    },
                ),
            )
            .add_stages(
                OfflineStages::default()
                    .set(SenderRecoveryStage {
                        batch_size: config.stages.sender_recovery.batch_size,
                        commit_threshold: config.stages.sender_recovery.commit_threshold,
                    })
                    .set(ExecutionStage {
                        chain_spec: self.chain.clone(),
                        commit_threshold: config.stages.execution.commit_threshold,
                    }),
            )
            .with_max_block(0)
            .build();

        let events = pipeline.events().map(Into::into);

        Ok((pipeline, events))
    }

    fn load_config(&self) -> eyre::Result<Config> {
        confy::load_path::<Config>(&self.config).wrap_err("Could not load config")
    }
}
