use crate::Log;
use reth_primitives::H256;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Response of the `eth_getFilterChanges` RPC.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilterChanges {
    /// New logs.
    Logs(Vec<Log>),
    /// New hashes (block or transactions)
    Hashes(Vec<H256>),
    /// Empty result,
    Empty,
}

impl Serialize for FilterChanges {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            FilterChanges::Logs(logs) => logs.serialize(s),
            FilterChanges::Hashes(hashes) => hashes.serialize(s),
            FilterChanges::Empty => (&[] as &[serde_json::Value]).serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for FilterChanges {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Changes {
            Logs(Vec<Log>),
            Hashes(Vec<H256>),
        }

        let changes = Changes::deserialize(deserializer)?;
        let changes = match changes {
            Changes::Logs(vals) => {
                if vals.is_empty() {
                    FilterChanges::Empty
                } else {
                    FilterChanges::Logs(vals)
                }
            }
            Changes::Hashes(vals) => {
                if vals.is_empty() {
                    FilterChanges::Empty
                } else {
                    FilterChanges::Hashes(vals)
                }
            }
        };
        Ok(changes)
    }
}
