//! Desk sessions: the real Claude Code and Codex terminals a hub runs for
//! the person at the machine, tracked through hooks and app-server events,
//! kept in a Postgres registry, and restored after a restart.

mod adopt;
pub mod api;
pub mod attachments;
mod background;
mod claude;
mod codex;
pub mod conversations;
pub mod crew;
mod crew_api;
mod crew_store;
mod cwd;
pub mod floor;
pub mod holder;
pub mod local;
pub mod model;
pub mod overview;
mod procs;
pub mod providers;
mod pty;
pub mod realtime;
pub mod rooms;
mod service;
pub mod speech;
pub mod state;
pub mod store;
#[cfg(test)]
mod testing;
mod transcript;
pub mod usage;
mod vt;
mod worktree;

pub use api::{router, sessions_router, Authorize};
pub use conversations::{
    Active, Author, Close, Closed, ConversationError, Conversations, Cursor, Page, Said, Speaker,
    SpeakerKind,
};
pub use crew_store::CheckIn;
pub use providers::{AccountError, Accounts, Fields, Spec, Standing};
pub use realtime::{KeySource, Provider, Realtime};
pub use service::{
    local_agent_item, session_item, Desk, DeskError, Floor, Group, HolderOptions, Import, Imported,
    Layout, NewTerminal, Options, RestorePolicy, TerminalPatch, TerminalState, TerminalView,
    Ungrouped, UNGROUPED,
};
pub use speech::{Audio, Speak, SpeakError, Speech, SpeechProvider};
pub use store::DeskStore;
