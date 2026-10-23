//! Process-wide hub instance identity.
//!
//! One stable id per process, shared by every subsystem that stamps ownership
//! into shared state (turn claims, workflow leases, instance heartbeats, stop
//! broadcasts). The nonce is fresh per boot, so a restarted process never
//! matches its predecessor's rows and cannot inherit stale ownership.

use std::sync::OnceLock;

/// `HOSTNAME-<8-hex nonce>`, or a bare UUID when `HOSTNAME` is unset.
pub fn process_instance_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let host = std::env::var("HOSTNAME").unwrap_or_default();
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        if host.is_empty() {
            nonce
        } else {
            format!("{host}-{}", &nonce[..8])
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_within_process() {
        assert_eq!(process_instance_id(), process_instance_id());
        assert!(!process_instance_id().is_empty());
    }
}
