//! How many results a node gives for one request.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::warn;

use crate::signals::most_results;

/// The most results a node gives for one request, unless it is run with
/// another `VELLEV_MAX_LIMIT`.
pub const MAX_RESULTS: usize = 1000;

/// The most results one request may ask of a node: [`MAX_RESULTS`] until
/// the node refuses that many, and then what it said it allows.
#[derive(Debug)]
pub struct Cap(AtomicUsize);

impl Default for Cap {
    fn default() -> Self {
        Self(AtomicUsize::new(MAX_RESULTS))
    }
}

impl Cap {
    /// `wanted` as a request's limit: at least one, at most the cap,
    /// warning when it is cut.
    pub fn limit(&self, wanted: usize) -> u32 {
        let most = self.0.load(Ordering::Relaxed);
        if wanted > most {
            warn!(
                limit = wanted,
                max = most,
                "limit cut to what vellev gives for one request"
            );
        }
        u32::try_from(wanted.clamp(1, most)).unwrap_or(u32::MAX)
    }

    /// `op` asked for `wanted` results, within the cap; if the node allows
    /// fewer than that, the cap becomes what it allows and `op` is asked
    /// again.
    pub async fn ask<T, Fut>(
        &self,
        wanted: usize,
        op: impl Fn(u32) -> Fut,
    ) -> vellev_client::Result<T>
    where
        Fut: Future<Output = vellev_client::Result<T>>,
    {
        let asked = self.limit(wanted);
        match op(asked).await {
            Err(e) => match most_results(&e).filter(|most| (*most as u64) < u64::from(asked)) {
                Some(most) => {
                    warn!(max = most, "vellev gives fewer results for one request than asked; asking for that many");
                    self.0.store(most, Ordering::Relaxed);
                    op(self.limit(wanted)).await
                }
                None => Err(e),
            },
            done => done,
        }
    }
}
