//! The worker daemon: serves the runner protocol on a host with local NVMe
//! under a ZFS pool, materialising each job's scope as clones of golden
//! datasets inside one container from the toolbox image.
//!
//! The crate is a functional core over four side-effect ports — [`Zfs`],
//! [`Containers`], [`Cloud`] and [`Repos`] — so every decision is testable
//! with in-memory fakes; the real ports shell out through the execution core
//! or call the GitHub API.

pub mod backup;
pub mod bridge;
pub mod config;
pub mod doctor;
pub mod handler;
pub mod host;
pub mod jobs;
pub mod ports;
pub mod refresh;
pub mod scopes;
pub mod serve;
pub mod state;

#[cfg(test)]
pub(crate) mod fakes;

pub use config::WorkerConfig;
pub use ports::{Cloud, Containers, Repos, Zfs};
pub use scopes::Worker;
