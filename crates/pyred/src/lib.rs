//! pyred library surface — exposed only for integration tests.
//!
//! This crate is primarily a binary (`pyred`). This lib target exists solely
//! to allow integration tests (e.g. `hybrid_smoke`) to call in-process
//! functions without spawning the binary.
//!
//! Do not add new public items here without a concrete test consumer.

pub mod config;
pub mod index;
pub mod migration;
pub mod shard;
pub mod store;
