//! Name-prompt overlay model types — extracted from main.rs (Wave 1F).

use pyre_proto::{SessionId, WindowId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptKind {
    NewSession,
    NewTab,
    RenameSession(SessionId),
    RenameWindow(WindowId),
}

pub(crate) struct NamePrompt {
    pub(crate) kind: PromptKind,
    pub(crate) input: String,
}
