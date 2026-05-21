//! Post-filters and snippet helpers for `search_blocks` RPC results.

use pyre_proto::{Block, BlockHit};

use crate::store::Store;

/// True when the block finished with a non-zero exit code.
pub fn block_is_failure(block: &Block) -> bool {
    block.exit_code.is_some_and(|c| c != 0)
}

/// Apply `failures_only` after tantivy search.
pub fn filter_hits(hits: Vec<BlockHit>, failures_only: bool) -> Vec<BlockHit> {
    if !failures_only {
        return hits;
    }
    hits.into_iter()
        .filter(|h| block_is_failure(&h.block))
        .collect()
}

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
