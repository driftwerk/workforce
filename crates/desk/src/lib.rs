//! Desk sessions: the real Claude Code and Codex terminals a hub runs for
//! the person at the machine, tracked through hooks and app-server events,
//! kept in a Postgres registry, and restored after a restart.

pub mod account;
pub mod account_store;
mod adopt;
pub mod api;
pub mod asking;
pub mod attachments;
pub mod away;
mod background;
pub mod board;
mod board_api;
pub mod board_store;
pub mod board_sync;
pub mod board_sync_store;
pub mod boss_ask;
pub mod boss_ask_store;
pub mod boss_shell;
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
pub mod decision;
pub mod decision_store;
pub mod engine;
pub mod engine_accounts;
mod first_run;
pub mod floor;
pub mod folders;
pub mod gist;
pub mod grant;
mod grant_api;
pub mod grant_store;
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
pub mod mode;
pub mod model;
mod move_api;
pub mod move_store;
pub mod moves;
pub mod never;
pub mod open_item;
pub mod open_item_store;
pub mod overview;
pub mod plan;
mod plan_api;
pub mod plan_store;
pub mod preference;
pub mod preference_store;
mod procs;
pub mod providers;
mod pty;
pub mod reads;
pub mod realtime;
pub mod rooms;
pub mod scope;
pub mod sees;
mod service;
pub mod speech;
pub mod state;
pub mod store;
#[cfg(test)]
mod testing;
mod token_pipe;
mod transcript;
pub mod update;
pub mod update_store;
pub mod usage;
mod vt;
mod worktree;

pub use adopt::HubInternal;
pub use api::{router, sessions_router, Authorize};
pub use conversations::{
    Active, Author, Close, Closed, ConversationError, ConversationRead, ConversationUnread,
    Conversations, Copied, CopyToChat, Cursor, Delete, Deleted, Match, Matches, Page, PutRead,
    Said, SavedMemory, Speaker, SpeakerKind, Thread, Threads,
};
pub use crew_store::CheckIn;
pub use providers::{AccountError, Accounts, Fields, Spec, Standing};
pub use realtime::{KeySource, Provider, Realtime};
pub use service::{
    agent_subject, plan_subject, pool_subject, session_subject, AgentSpend, AgentVerdict, Boss,
    BreakerOf, BreakerView, BudgetPatch, BudgetView, BudgetsView, ChargeView, NewBudget, Rules,
};
pub use service::{check_hub_id, check_hub_url, LinkBoard};
pub use service::{
    local_agent_item, session_item, Clearing, Desk, DeskError, Floor, Group, Hidden, HolderOptions,
    Import, Imported, Layout, LedgerPage, LineError, MemoryHitView, MemorySearch, NewTerminal,
    Options, Removal, Removed, RestorePolicy, Sender as LineSender, SessionMcp, SessionMemory,
    SpendQuery, TerminalPatch, TerminalState, TerminalView, Ungrouped, UNGROUPED,
};
pub use service::{AccountAnswer, AccountMoment, AccountRead};
pub use service::{
    AssignTo, BoardPatch, ChargedTo, Move, NewBoard, NewSessionFor, NewTicket, TicketPatch,
    TicketQuery, TicketView,
};
pub use service::{
    Changed, NewPlan, OnDrop, PlanDetail, PlanEdit, PlanSummary, PlanTicketView, StartRefused,
};
pub use service::{EffectView, HandoverView, MoveAsk, MoveCommit, MoveDone, MovePreview, SentNote};
pub use service::{GrantView, GrantWritten, GrantsView, Native, Paused};
pub use service::{HandedOff, HandoffPreview, Hop, LineageView};
pub use speech::{Audio, Speak, SpeakError, Speech, SpeechProvider, VoiceKey};
pub use store::DeskStore;
pub use token_pipe::private_run_dir;
