//! Witness ingest pipeline: extracts Orchard note commitments and decryption
//! data from compact blocks and feeds them to the commitment tree and
//! decryption PIR database.
//!
//! Mirrors `nf-ingest` structurally but focuses on `cmx` values and decryption
//! leaves rather than nullifiers. Uses [`chain_ingest::LwdClient`] and
//! [`chain_ingest::ChainTracker`] for lightwalletd communication and reorg
//! detection.
//!
//! # Modules
//!
//! - [`parser`]: Extracts `cmx` values, decryption leaves, and tree size
//!   metadata from compact blocks.
//! - [`ingest`]: Sync and follow loops that emit [`WitnessChainEvent`]s.

pub mod ingest;
pub mod parser;

pub use parser::{
    extract_block_commitments, extract_commitments, extract_commitments_and_decryption,
    extract_decryption_leaves, orchard_tree_size, BlockCommitments,
};
