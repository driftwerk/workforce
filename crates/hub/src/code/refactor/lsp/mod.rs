//! Language servers behind `code.refactor`'s rename, diagnostics and
//! navigation.

pub mod edit;
#[cfg(test)]
pub mod fake;
pub mod pool;
pub mod rpc;
pub mod server;

pub use pool::{Lease, Pool};
pub use server::{
    path_of, uri_of, Document, LanguageServer, Launcher, LspError, ProcessLauncher, ServerKind,
    ServerSpec, Timeouts,
};
