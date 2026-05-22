//! GPU layout shim — re-exports the canonical `LayoutNode` from `pyre-proto`
//! and keeps only the GPU-specific `contains_pt` helper for paint hit-testing.
//!
//! All unit tests have been migrated to `pyre_proto::layout` (M7-B).

// Re-export everything callers previously imported from this module.
pub use pyre_proto::layout::{Dir, LayoutNode, Orient, Rect};
