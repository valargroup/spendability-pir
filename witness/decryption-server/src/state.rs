use arc_swap::ArcSwap;
use decryption_types::*;
use pir_types::{PirEngine, ServerPhase, YpirScenario};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecryptionMetadata {
    pub anchor_height: u64,
    pub tree_size: u64,
    pub window_start_shard: u32,
    pub window_shard_count: u32,
    pub populated_shards: u32,
    pub phase: ServerPhase,
}

pub struct PirState<P: PirEngine> {
    pub engine_state: P::ServerState,
    pub metadata: DecryptionMetadata,
}

pub struct AppState<P: PirEngine> {
    pub live_pir: ArcSwap<Option<PirState<P>>>,
    pub phase: ArcSwap<ServerPhase>,
    pub scenario: YpirScenario,
    pub engine: Arc<P>,
}

impl<P: PirEngine> AppState<P> {
    pub fn new(engine: Arc<P>) -> Self {
        Self {
            live_pir: ArcSwap::from_pointee(None),
            phase: ArcSwap::from_pointee(ServerPhase::Syncing {
                current_height: 0,
                target_height: 0,
            }),
            scenario: YpirScenario {
                num_items: DECRYPT_DB_ROWS as u64,
                item_size_bits: (DECRYPT_ROW_BYTES * 8) as u64,
            },
            engine,
        }
    }
}
