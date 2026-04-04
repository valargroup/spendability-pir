//! Witness ingest pipeline: extracts Orchard note commitments from compact
//! blocks and feeds them to the commitment tree.
//!
//! Mirrors `nf-ingest` structurally but focuses on `cmx` values rather than
//! nullifiers. Uses [`chain_ingest::LwdClient`] and [`chain_ingest::ChainTracker`]
//! for lightwalletd communication and reorg detection.
//!
//! # Modules
//!
//! - [`parser`]: Extracts `cmx` values and tree size metadata from compact blocks.
//! - [`ingest`]: Sync and follow loops that emit [`WitnessChainEvent`]s.

pub mod ingest;
pub mod parser;

pub use parser::{
    extract_block_commitments, extract_commitments, orchard_tree_size, BlockCommitments,
};
