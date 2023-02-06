//! Configuration files.
use std::sync::Arc;

use reth_db::database::Database;
use reth_discv4::Discv4Config;
use reth_network::{
    config::{mainnet_nodes, rng_secret_key},
    NetworkConfig, NetworkConfigBuilder, PeersConfig,
};
use reth_primitives::{ChainSpec, NodeRecord};
use reth_provider::ShareableDatabase;
use serde::{Deserialize, Serialize};

/// Configuration for the reth node.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Configuration for each stage in the pipeline.
    // TODO(onbjerg): Can we make this easier to maintain when we add/remove stages?
    pub stages: StageConfig,
    /// Configuration for the discovery service.
    pub peers: PeersConfig,
}

impl Config {
    /// Initializes network config from read data
    pub fn network_config<DB: Database>(
        &self,
        db: DB,
        chain_spec: ChainSpec,
        disable_discovery: bool,
        bootnodes: Option<Vec<NodeRecord>>,
        nat_resolution_method: reth_net_nat::NatResolver,
    ) -> NetworkConfig<ShareableDatabase<DB>> {
        let peer_config = reth_network::PeersConfig::default()
            .with_trusted_nodes(self.peers.trusted_nodes.clone())
            .with_connect_trusted_nodes_only(self.peers.connect_trusted_nodes_only);
        let discv4 =
            Discv4Config::builder().external_ip_resolver(Some(nat_resolution_method)).clone();
        NetworkConfigBuilder::new(rng_secret_key())
            .boot_nodes(bootnodes.unwrap_or_else(mainnet_nodes))
            .peer_config(peer_config)
            .discovery(discv4)
            .chain_spec(chain_spec)
            .set_discovery(disable_discovery)
            .build(Arc::new(ShareableDatabase::new(db)))
    }
}

/// Configuration for each stage in the pipeline.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StageConfig {
    /// Header stage configuration.
    pub headers: HeadersConfig,
    /// Total difficulty stage configuration
    pub total_difficulty: TotalDifficultyConfig,
    /// Body stage configuration.
    pub bodies: BodiesConfig,
    /// Sender recovery stage configuration.
    pub sender_recovery: SenderRecoveryConfig,
    /// Execution stage configuration.
    pub execution: ExecutionConfig,
}

/// Header stage configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HeadersConfig {
    /// The maximum number of headers to download before committing progress to the database.
    pub commit_threshold: u64,
    /// The maximum number of headers to request from a peer at a time.
    pub downloader_batch_size: u64,
    /// The number of times to retry downloading a set of headers.
    pub downloader_retries: usize,
}

impl Default for HeadersConfig {
    fn default() -> Self {
        Self { commit_threshold: 10_000, downloader_batch_size: 1000, downloader_retries: 5 }
    }
}

/// Total difficulty stage configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TotalDifficultyConfig {
    /// The maximum number of total difficulty entries to sum up before committing progress to the
    /// database.
    pub commit_threshold: u64,
}

impl Default for TotalDifficultyConfig {
    fn default() -> Self {
        Self { commit_threshold: 100_000 }
    }
}

/// Body stage configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BodiesConfig {
    /// The batch size of non-empty blocks per one request
    pub downloader_request_limit: u64,
    /// The maximum number of block bodies returned at once from the stream
    pub downloader_stream_batch_size: usize,
    /// Maximum amount of received bodies to buffer internally.
    pub downloader_max_buffered_responses: usize,
    /// The minimum number of requests to send concurrently.
    pub downloader_min_concurrent_requests: usize,
    /// The maximum number of requests to send concurrently.
    pub downloader_max_concurrent_requests: usize,
}

impl Default for BodiesConfig {
    fn default() -> Self {
        Self {
            downloader_request_limit: 200,
            downloader_stream_batch_size: 10000,
            downloader_max_buffered_responses: 30000,
            downloader_min_concurrent_requests: 5,
            downloader_max_concurrent_requests: 100,
        }
    }
}

/// Sender recovery stage configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SenderRecoveryConfig {
    /// The maximum number of blocks to process before committing progress to the database.
    pub commit_threshold: u64,
    /// The maximum number of transactions to recover senders for concurrently.
    pub batch_size: usize,
}

impl Default for SenderRecoveryConfig {
    fn default() -> Self {
        Self { commit_threshold: 5_000, batch_size: 1000 }
    }
}

/// Execution stage configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExecutionConfig {
    /// The maximum number of blocks to execution before committing progress to the database.
    pub commit_threshold: u64,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self { commit_threshold: 5_000 }
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    #[test]
    fn can_serde_config() {
        let _: Config = confy::load("test", None).unwrap();
    }
}
