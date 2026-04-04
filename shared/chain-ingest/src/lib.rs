//! Shared chain ingest infrastructure for PIR services.
//!
//! Provides the lightwalletd gRPC client ([`LwdClient`]), reorg-detecting
//! chain tracker ([`ChainTracker`], [`ChainAction`]), and protobuf types
//! used by both nullifier and witness ingest pipelines.
//!
//! Each subsystem (nullifier, witness) writes its own parser and sync/follow
//! loop on top of these shared primitives, extracting different data from
//! the same compact block stream.

pub mod chain_tracker;
pub mod client;
pub mod proto;

pub use chain_tracker::{ChainAction, ChainTracker};
pub use client::{ClientError, LwdClient};
