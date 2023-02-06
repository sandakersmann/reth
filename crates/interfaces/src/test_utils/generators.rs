use rand::{distributions::uniform::SampleRange, thread_rng, Rng};
use reth_primitives::{
    proofs, Account, Address, Bytes, Header, SealedBlock, SealedHeader, Signature, Transaction,
    TransactionKind, TransactionSigned, TxLegacy, H160, H256, U256,
};
use secp256k1::{KeyPair, Message as SecpMessage, Secp256k1, SecretKey};

// TODO(onbjerg): Maybe we should split this off to its own crate, or move the helpers to the
// relevant crates?

/// Generates a range of random [SealedHeader]s.
///
/// The parent hash of the first header
/// in the result will be equal to `head`.
///
/// The headers are assumed to not be correct if validated.
pub fn random_header_range(rng: std::ops::Range<u64>, head: H256) -> Vec<SealedHeader> {
    let mut headers = Vec::with_capacity(rng.end.saturating_sub(rng.start) as usize);
    for idx in rng {
        headers.push(random_header(
            idx,
            Some(headers.last().map(|h: &SealedHeader| h.hash()).unwrap_or(head)),
        ));
    }
    headers
}

/// Generate a random [SealedHeader].
///
/// The header is assumed to not be correct if validated.
pub fn random_header(number: u64, parent: Option<H256>) -> SealedHeader {
    let header = reth_primitives::Header {
        number,
        nonce: rand::random(),
        difficulty: U256::from(rand::random::<u32>()),
        parent_hash: parent.unwrap_or_default(),
        ..Default::default()
    };
    header.seal()
}

/// Generates a random legacy [Transaction].
///
/// Every field is random, except:
///
/// - The chain ID, which is always 1
/// - The input, which is always nothing
pub fn random_tx() -> Transaction {
    Transaction::Legacy(TxLegacy {
        chain_id: Some(1),
        nonce: rand::random::<u16>().into(),
        gas_price: rand::random::<u16>().into(),
        gas_limit: rand::random::<u16>().into(),
        to: TransactionKind::Call(Address::random()),
        value: rand::random::<u16>().into(),
        input: Bytes::default(),
    })
}

/// Generates a random legacy [Transaction] that is signed.
///
/// On top of the considerations of [random_tx], these apply as well:
///
/// - There is no guarantee that the nonce is not used twice for the same account
pub fn random_signed_tx() -> TransactionSigned {
    let secp = Secp256k1::new();
    let key_pair = KeyPair::new(&secp, &mut rand::thread_rng());
    let tx = random_tx();
    let signature =
        sign_message(H256::from_slice(&key_pair.secret_bytes()[..]), tx.signature_hash()).unwrap();
    TransactionSigned::from_transaction_and_signature(tx, signature)
}

/// Signs message with the given secret key.
/// Returns the corresponding signature.
pub fn sign_message(secret: H256, message: H256) -> Result<Signature, secp256k1::Error> {
    let secp = Secp256k1::new();
    let sec = SecretKey::from_slice(secret.as_ref())?;
    let s = secp.sign_ecdsa_recoverable(&SecpMessage::from_slice(&message[..])?, &sec);
    let (rec_id, data) = s.serialize_compact();

    Ok(Signature {
        r: U256::try_from_be_slice(&data[..32]).unwrap(),
        s: U256::try_from_be_slice(&data[32..64]).unwrap(),
        odd_y_parity: rec_id.to_i32() != 0,
    })
}

/// Generate a random block filled with signed transactions (generated using
/// [random_signed_tx]). If no transaction count is provided, the number of transactions
/// will be random, otherwise the provided count will be used.
///
/// All fields use the default values (and are assumed to be invalid) except for:
///
/// - `parent_hash`
/// - `transactions_root`
/// - `ommers_hash`
///
/// Additionally, `gas_used` and `gas_limit` always exactly match the total `gas_limit` of all
/// transactions in the block.
///
/// The ommer headers are not assumed to be valid.
pub fn random_block(
    number: u64,
    parent: Option<H256>,
    tx_count: Option<u8>,
    ommers_count: Option<u8>,
) -> SealedBlock {
    let mut rng = thread_rng();

    // Generate transactions
    let tx_count = tx_count.unwrap_or_else(|| rng.gen::<u8>());
    let transactions: Vec<TransactionSigned> = (0..tx_count).map(|_| random_signed_tx()).collect();
    let total_gas = transactions.iter().fold(0, |sum, tx| sum + tx.transaction.gas_limit());

    // Generate ommers
    let ommers_count = ommers_count.unwrap_or_else(|| rng.gen_range(0..2));
    let ommers =
        (0..ommers_count).map(|_| random_header(number, parent).unseal()).collect::<Vec<_>>();

    // Calculate roots
    let transactions_root = proofs::calculate_transaction_root(transactions.iter());
    let ommers_hash = proofs::calculate_ommers_root(ommers.iter());

    SealedBlock {
        header: Header {
            parent_hash: parent.unwrap_or_default(),
            number,
            gas_used: total_gas,
            gas_limit: total_gas,
            transactions_root,
            ommers_hash,
            base_fee_per_gas: Some(rng.gen()),
            ..Default::default()
        }
        .seal(),
        body: transactions,
        ommers: ommers.into_iter().map(|ommer| ommer.seal()).collect(),
    }
}

/// Generate a range of random blocks.
///
/// The parent hash of the first block
/// in the result will be equal to `head`.
///
/// See [random_block] for considerations when validating the generated blocks.
pub fn random_block_range(
    block_numbers: std::ops::Range<u64>,
    head: H256,
    tx_count: std::ops::Range<u8>,
) -> Vec<SealedBlock> {
    let mut rng = rand::thread_rng();
    let mut blocks =
        Vec::with_capacity(block_numbers.end.saturating_sub(block_numbers.start) as usize);
    for idx in block_numbers {
        blocks.push(random_block(
            idx,
            Some(blocks.last().map(|block: &SealedBlock| block.header.hash()).unwrap_or(head)),
            Some(tx_count.clone().sample_single(&mut rng)),
            None,
        ));
    }
    blocks
}

/// Generate random Externaly Owned Account (EOA account without contract).
pub fn random_eoa_account() -> (Address, Account) {
    let nonce: u64 = rand::random();
    let balance = U256::from(rand::random::<u32>());
    let addr = H160::from(rand::random::<u64>());

    (addr, Account { nonce, balance, bytecode_hash: None })
}

/// Generate random Externaly Owned Accounts
pub fn random_eoa_account_range(acc_range: &mut std::ops::Range<u64>) -> Vec<(Address, Account)> {
    let mut accounts = Vec::with_capacity(acc_range.end.saturating_sub(acc_range.start) as usize);
    for _ in acc_range {
        accounts.push(random_eoa_account())
    }
    accounts
}

/// Generate random Contract Accounts
pub fn random_contract_account_range(
    acc_range: &mut std::ops::Range<u64>,
) -> Vec<(Address, Account)> {
    let mut accounts = Vec::with_capacity(acc_range.end.saturating_sub(acc_range.start) as usize);
    for _ in acc_range {
        let (address, eoa_account) = random_eoa_account();
        let account = Account { bytecode_hash: Some(H256::random()), ..eoa_account };
        accounts.push((address, account))
    }
    accounts
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use super::*;
    use hex_literal::hex;
    use reth_primitives::{keccak256, AccessList, Address, TransactionKind, TxEip1559};
    use secp256k1::KeyPair;

    #[test]
    fn test_sign_message() {
        let secp = Secp256k1::new();

        let tx = Transaction::Eip1559(TxEip1559 {
            chain_id: 1,
            nonce: 0x42,
            gas_limit: 44386,
            to: TransactionKind::Call(hex!("6069a6c32cf691f5982febae4faf8a6f3ab2f0f6").into()),
            value: 0_u128,
            input:  hex!("a22cb4650000000000000000000000005eee75727d804a2b13038928d36f8b188945a57a0000000000000000000000000000000000000000000000000000000000000000").into(),
            max_fee_per_gas: 0x4a817c800,
            max_priority_fee_per_gas: 0x3b9aca00,
            access_list: AccessList::default(),
        });
        let signature_hash = tx.signature_hash();

        for _ in 0..100 {
            let key_pair = KeyPair::new(&secp, &mut rand::thread_rng());

            let signature =
                sign_message(H256::from_slice(&key_pair.secret_bytes()[..]), signature_hash)
                    .unwrap();

            let signed = TransactionSigned::from_transaction_and_signature(tx.clone(), signature);
            let recovered = signed.recover_signer().unwrap();

            let public_key_hash = keccak256(&key_pair.public_key().serialize_uncompressed()[1..]);
            let expected = Address::from_slice(&public_key_hash[12..]);

            assert_eq!(recovered, expected);
        }
    }

    #[test]
    fn test_sign_eip_155() {
        // reference: https://github.com/ethereum/EIPs/blob/master/EIPS/eip-155.md#example
        let transaction = Transaction::Legacy(TxLegacy {
            chain_id: Some(1),
            nonce: 9,
            gas_price: 20 * 10_u128.pow(9),
            gas_limit: 21000,
            to: TransactionKind::Call(hex!("3535353535353535353535353535353535353535").into()),
            value: 10_u128.pow(18),
            input: Bytes::default(),
        });

        // TODO resolve dependency issue
        // let mut encoded = BytesMut::new();
        // transaction.encode(&mut encoded);
        // let expected =
        // hex!("ec098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a764000080018080");
        // assert_eq!(expected, encoded.as_ref());

        let hash = transaction.signature_hash();
        let expected =
            H256::from_str("daf5a779ae972f972197303d7b574746c7ef83eadac0f2791ad23db92e4c8e53")
                .unwrap();
        assert_eq!(expected, hash);

        let secret =
            H256::from_str("4646464646464646464646464646464646464646464646464646464646464646")
                .unwrap();
        let signature = sign_message(secret, hash).unwrap();

        let expected = Signature {
            r: U256::from_str(
                "18515461264373351373200002665853028612451056578545711640558177340181847433846",
            )
            .unwrap(),
            s: U256::from_str(
                "46948507304638947509940763649030358759909902576025900602547168820602576006531",
            )
            .unwrap(),
            odd_y_parity: false,
        };
        assert_eq!(expected, signature);
    }
}
