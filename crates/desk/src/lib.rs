//! Desk sessions: the real Claude Code and Codex terminals a hub runs for
//! the person at the machine, tracked through hooks and app-server events,
//! kept in a Postgres registry, and restored after a restart.

mod adopt;
pub mod api;
mod claude;
mod codex;
pub mod model;
mod pty;
pub mod realtime;
pub mod rooms;
mod service;
pub mod store;
mod transcript;
mod worktree;

pub use api::{router, Authorize};
pub use realtime::{KeySource, Provider, Realtime};
pub use service::{Desk, DeskError, Options, RestorePolicy};
pub use store::DeskStore;
