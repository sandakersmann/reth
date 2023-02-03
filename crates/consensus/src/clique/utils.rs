//! Utility methods for clique consensus.

use super::constants::EXTRA_SEAL;
use reth_interfaces::consensus::CliqueError;
use reth_primitives::{recovery::secp256k1, Address, SealedHeader};

/// Recover the account from signed header per clique consensus rules.
pub fn recover_header_signer(header: &SealedHeader) -> Result<Address, CliqueError> {
    let extra_data_len = header.extra_data.len();
    let signature = extra_data_len
        .checked_sub(EXTRA_SEAL)
        .and_then(|start| -> Option<[u8; 65]> { header.extra_data[start..].try_into().ok() })
        .ok_or(CliqueError::MissingSignature { extra_data: header.extra_data.clone() })?;
    secp256k1::recover(&signature, header.hash().as_fixed_bytes())
        .map_err(|_| CliqueError::HeaderSignerRecovery { signature, hash: header.hash() })
}
