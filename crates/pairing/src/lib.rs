pub mod channel;
pub mod code;
pub mod device;
pub mod node;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub mod postgres_migrate;
pub mod store;
pub mod types;

pub use channel::{ChannelPairingService, SenderCheckResult};
pub use device::DevicePairingService;
pub use node::NodePairingService;
#[cfg(feature = "postgres")]
pub use postgres::PostgresPairingStore;
pub use store::{PairingStore, SqlitePairingStore};
pub use types::*;

/// What a test's code logs, for tests that a log never says a secret.
#[cfg(test)]
pub(crate) fn logged(run: impl FnOnce()) -> String {
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let log = Captured::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    // Tests run on parallel threads with no subscriber of their own: a
    // callsite first hit there is cached as unwanted, so the cache is rebuilt
    // once this one is in place.
    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        run()
    });
    let bytes = log.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}
