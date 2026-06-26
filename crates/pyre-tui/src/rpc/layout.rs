use pyre_proto::{layout::LayoutNode, PyreDaemonClient, SessionId, WindowId};

// ─────────────────────────────────────────────────────────────────────────────
// Layout RPC helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Fetch the authoritative layout for `session_id` from the daemon.
/// Returns `None` if the RPC fails or the daemon returns an error.
pub async fn get_session_layout(
    client: &PyreDaemonClient,
    session_id: SessionId,
) -> Option<LayoutNode> {
    client
        .get_session_layout(tarpc::context::current(), session_id)
        .await
        .ok()
        .and_then(|r| r.ok())
}

/// Fetch the authoritative layout for `window_id` from the daemon.
/// Returns `None` if the RPC fails or the daemon returns an error.
pub async fn get_window_layout(
    client: &PyreDaemonClient,
    window_id: WindowId,
) -> Option<LayoutNode> {
    client
        .get_window_layout(tarpc::context::current(), window_id)
        .await
        .ok()
        .and_then(|r| r.ok())
}
