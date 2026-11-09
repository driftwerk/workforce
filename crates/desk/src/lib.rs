//! Desk sessions: the real Claude Code and Codex terminals a hub runs for
//! the person at the machine, tracked through hooks and app-server events,
//! kept in a Postgres registry, and restored after a restart.

mod adopt;
pub mod api;
pub mod asking;
pub mod attachments;
mod background;
pub mod board;
mod board_api;
pub mod board_store;
pub mod breaker;
pub mod budget_store;
pub mod charge;
mod charge_api;
pub mod charge_store;
mod claude;
mod codex;
mod codex_session;
pub mod computer;
pub mod connections;
pub mod conversations;
pub mod crew;
mod crew_api;
mod crew_store;
mod cwd;
pub mod engine;
pub mod engine_accounts;
mod first_run;
pub mod floor;
pub mod folders;
pub mod gist;
pub mod handoff;
pub mod holder;
pub mod identity;
pub mod leader;
pub mod ledger;
pub mod ledger_store;
pub mod limit;
pub mod line;
mod line_api;
pub mod line_store;
pub mod lineage_store;
pub mod local;
pub mod memory;
pub mod memory_derive_store;
pub mod memory_index_store;
pub mod memory_store;
pub mod model;
mod move_api;
pub mod move_store;
pub mod moves;
pub mod overview;
pub mod plan;
mod plan_api;
pub mod plan_store;
mod procs;
pub mod providers;
mod pty;
pub mod reads;
pub mod realtime;
pub mod rooms;
pub mod scope;
mod service;
pub mod speech;
pub mod state;
pub mod store;
#[cfg(test)]
mod testing;
mod token_pipe;
mod transcript;
pub mod usage;
mod vt;
mod worktree;

pub use adopt::HubInternal;
pub use api::{router, sessions_router, Authorize};
pub use conversations::{
    Active, Author, Close, Closed, ConversationError, ConversationRead, ConversationUnread,
    Conversations, Cursor, Page, PutRead, Said, Speaker, SpeakerKind,
};
pub use crew_store::CheckIn;
pub use providers::{AccountError, Accounts, Fields, Spec, Standing};
pub use realtime::{KeySource, Provider, Realtime};
pub use service::{
    agent_subject, plan_subject, pool_subject, session_subject, AgentSpend, AgentVerdict, Boss,
    BreakerOf, BreakerView, BudgetPatch, BudgetView, BudgetsView, ChargeView, NewBudget, Rules,
};
pub use service::{
    local_agent_item, session_item, Clearing, Desk, DeskError, Floor, Group, Hidden, HolderOptions,
    Import, Imported, Layout, LedgerPage, LineError, MemoryHitView, MemorySearch, NewTerminal,
    Options, Removal, Removed, RestorePolicy, Sender as LineSender, SessionMcp, SessionMemory,
    SpendQuery, TerminalPatch, TerminalState, TerminalView, Ungrouped, UNGROUPED,
};
pub use service::{
    AssignTo, BoardPatch, ChargedTo, Move, NewBoard, NewSessionFor, NewTicket, TicketPatch,
    TicketQuery, TicketView,
};
pub use service::{
    Changed, NewPlan, OnDrop, PlanDetail, PlanEdit, PlanSummary, PlanTicketView, StartRefused,
};
pub use service::{EffectView, HandoverView, MoveAsk, MoveCommit, MoveDone, MovePreview, SentNote};
pub use service::{HandedOff, HandoffPreview, Hop, LineageView};
pub use speech::{Audio, Speak, SpeakError, Speech, SpeechProvider};
pub use store::DeskStore;
pub use token_pipe::private_run_dir;
