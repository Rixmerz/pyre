use pyre_proto::SessionId;

use crate::app::sessions::SessionView;

// ─────────────────────────────────────────────────────────────────────────────
// Active session preservation helpers
// ─────────────────────────────────────────────────────────────────────────────

/// After mutations to the sessions list (inserts, removals) that may shift
/// indices, restore `active_session` to the index of the session the user was
/// viewing before the mutation batch.
///
/// - If `prev_active_id` is still present, point `active_session` at it.
/// - If it was pruned and other sessions remain, fall back to the last one.
/// - If `active_session` is out of bounds for any other reason, clamp it.
pub fn restore_active_session(
    sessions: &[SessionView],
    active_session: &mut usize,
    prev_active_id: Option<SessionId>,
) {
    if let Some(id) = prev_active_id {
        if let Some(new_idx) = sessions.iter().position(|sv| sv.id == id) {
            *active_session = new_idx;
        } else if !sessions.is_empty() {
            // The session the user was on no longer exists — pick last.
            *active_session = sessions.len() - 1;
        }
    } else if *active_session >= sessions.len() && !sessions.is_empty() {
        *active_session = sessions.len() - 1;
    }
}
