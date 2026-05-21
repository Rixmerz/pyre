//! Snippet helpers for `search_blocks` RPC results.
//!
//! `filter_hits` / `block_is_failure` were removed in schema v2: the
//! `failures_only` flag is now handled at the Tantivy layer via a
//! `RangeQuery` on the `exit_code` FAST field. See `index.rs`.

use pyre_proto::{Block, BlockHit};

use crate::store::Store;

/// Build `BlockHit` rows with stdout snippets from the store.
pub fn hits_with_snippets(store: &Store, blocks: Vec<Block>, max_chars: usize) -> Vec<BlockHit> {
    blocks
        .into_iter()
        .map(|block| {
            let snippet = store.stdout_snippet(block.id, max_chars);
            BlockHit { block, snippet }
        })
        .collect()
}
