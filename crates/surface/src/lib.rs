//! Which listener a request arrived on, and what each listener serves. Each
//! listener runs its requests in an [`Arrival`] scope; code that reads
//! [`current`] outside one (a spawned task, a background job) gets the hub's
//! default, which [`set_default`] fixes once before any listener binds. The
//! routes it serves are the [`Routes`] rendered for its [`Surface`].

mod routes;
pub mod routing;

pub use routes::{Bound, Declared, RouteClass, Routes};

use std::future::Future;
use std::net::IpAddr;
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Surface {
    Combined,
    Public,
    Admin,
}

/// The surface a request arrived on and the TCP peer it came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Arrival {
    pub surface: Surface,
    pub peer: Option<IpAddr>,
}

tokio::task_local! {
    static ARRIVAL: Arrival;
}

static DEFAULT: OnceLock<Surface> = OnceLock::new();

impl Arrival {
    pub fn new(surface: Surface, peer: Option<IpAddr>) -> Self {
        Self { surface, peer }
    }

    /// Runs `fut` as a request that arrived this way.
    pub async fn scope<F: Future>(self, fut: F) -> F::Output {
        ARRIVAL.scope(self, fut).await
    }
}

/// Fixes the surface of everything outside an [`Arrival`] scope; the first
/// call wins, and a later one hands back the surface it could not set.
pub fn set_default(surface: Surface) -> Result<(), Surface> {
    DEFAULT.set(surface)
}

/// The arrival in scope, else the default surface with no peer: `Combined`
/// until [`set_default`] says otherwise.
pub fn current() -> Arrival {
    resolve(
        ARRIVAL.try_with(|arrival| *arrival).ok(),
        DEFAULT.get().copied(),
    )
}

fn resolve(scoped: Option<Arrival>, default: Option<Surface>) -> Arrival {
    scoped.unwrap_or_else(|| Arrival::new(default.unwrap_or(Surface::Combined), None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const LOOPBACK: Option<IpAddr> = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));

    #[test]
    fn a_scope_outranks_the_default() {
        let scoped = Arrival::new(Surface::Admin, LOOPBACK);
        assert_eq!(resolve(Some(scoped), Some(Surface::Public)), scoped);
        assert_eq!(resolve(Some(scoped), None), scoped);
    }

    #[test]
    fn outside_a_scope_the_default_holds_with_no_peer() {
        assert_eq!(
            resolve(None, Some(Surface::Public)),
            Arrival::new(Surface::Public, None)
        );
        assert_eq!(resolve(None, None), Arrival::new(Surface::Combined, None));
    }

    #[tokio::test]
    async fn a_request_reads_its_arrival_and_a_task_it_spawns_reads_the_default() {
        let arrival = Arrival::new(Surface::Admin, LOOPBACK);
        let (inside, spawned) = arrival
            .scope(async { (current(), tokio::spawn(async { current() }).await.unwrap()) })
            .await;
        assert_eq!(inside, arrival);
        assert_eq!(spawned, Arrival::new(Surface::Combined, None));
        assert_eq!(current(), Arrival::new(Surface::Combined, None));
    }
}
