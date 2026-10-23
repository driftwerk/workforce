//! Sliding-window rate limiter for authentication endpoints.
//!
//! Tracks failed auth attempts per key and returns 429 Too Many Requests when
//! the threshold is exceeded. Includes automatic lockout and periodic cleanup.
//!
//! Password logins bucket on a [`CredentialKey`] — the attempted account scoped
//! to the client IP — rather than the IP alone, so one account's failures never
//! lock out everyone when a reverse proxy or overlay network collapses all
//! clients to a single source IP.

use std::hash::Hash;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

/// Rate limiter configuration.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum failed attempts before lockout.
    pub max_attempts: u32,
    /// Sliding window duration.
    pub window: Duration,
    /// Lockout duration after exceeding max_attempts.
    pub lockout: Duration,
    /// Exempt loopback addresses (127.0.0.1, ::1).
    pub exempt_loopback: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(300),
            exempt_loopback: true,
        }
    }
}

/// Key a [`RateLimiter`] can bucket on. The `is_loopback` hook lets a limiter
/// honour [`RateLimitConfig::exempt_loopback`] whether the key is a bare IP or
/// a composite that carries one.
pub trait RateLimitKey: Clone + Eq + Hash {
    fn is_loopback(&self) -> bool {
        false
    }
}

impl RateLimitKey for IpAddr {
    fn is_loopback(&self) -> bool {
        IpAddr::is_loopback(self)
    }
}

/// Bucket for password logins: the attempted account scoped to the client IP.
/// Keying on the account keeps one user's failures from locking out everyone
/// when a proxy or overlay network collapses all clients to one source IP.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CredentialKey {
    ip: IpAddr,
    email: String,
}

impl CredentialKey {
    pub fn new(ip: IpAddr, email: &str) -> Self {
        Self {
            ip,
            email: email.trim().to_ascii_lowercase(),
        }
    }
}

impl RateLimitKey for CredentialKey {
    fn is_loopback(&self) -> bool {
        self.ip.is_loopback()
    }
}

/// Per-key tracking entry.
struct Entry {
    /// Timestamps of failed attempts within the window.
    attempts: Vec<Instant>,
    /// If locked out, when the lockout expires.
    locked_until: Option<Instant>,
}

/// In-memory sliding-window rate limiter keyed on `K`.
pub struct RateLimiter<K: RateLimitKey> {
    config: RateLimitConfig,
    entries: DashMap<K, Entry>,
}

impl<K: RateLimitKey> RateLimiter<K> {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            entries: DashMap::new(),
        }
    }

    /// Check whether `key` is allowed to make a request.
    /// Returns `Ok(())` if allowed, `Err(retry_after_secs)` if rate limited.
    ///
    /// Only reads existing buckets — an unknown key is always allowed and
    /// never allocates. Buckets are created solely by [`Self::record_failure`],
    /// so an unauthenticated caller cannot grow the map by probing keys that
    /// never fail; entry creation is bounded to the recorded-failure path.
    pub fn check(&self, key: &K) -> Result<(), u64> {
        if self.config.exempt_loopback && key.is_loopback() {
            return Ok(());
        }

        let Some(mut entry) = self.entries.get_mut(key) else {
            return Ok(());
        };

        let now = Instant::now();
        if let Some(locked_until) = entry.locked_until {
            if now < locked_until {
                let remaining = locked_until.duration_since(now).as_secs();
                return Err(remaining.max(1));
            }
            entry.locked_until = None;
            entry.attempts.clear();
        }

        let window_start = now - self.config.window;
        entry.attempts.retain(|t| *t > window_start);

        if entry.attempts.len() >= self.config.max_attempts as usize {
            entry.locked_until = Some(now + self.config.lockout);
            Err(self.config.lockout.as_secs())
        } else {
            Ok(())
        }
    }

    /// Record a failed authentication attempt for `key`.
    pub fn record_failure(&self, key: K) {
        if self.config.exempt_loopback && key.is_loopback() {
            return;
        }

        let now = Instant::now();
        let mut entry = self.entries.entry(key).or_insert_with(|| Entry {
            attempts: Vec::new(),
            locked_until: None,
        });
        entry.attempts.push(now);
    }

    /// Remove stale entries (called periodically).
    pub fn cleanup(&self) {
        let now = Instant::now();
        self.entries.retain(|_key, entry| {
            if let Some(locked_until) = entry.locked_until {
                if now < locked_until {
                    return true;
                }
            }
            let window_start = now - self.config.window;
            entry.attempts.retain(|t| *t > window_start);
            !entry.attempts.is_empty()
        });
    }
}

/// Shared rate limiter handle.
pub type SharedRateLimiter<K> = Arc<RateLimiter<K>>;

/// Sliding-window limiter keyed on a caller identity (string), used to
/// throttle expensive **authenticated** operations like chat, vault writes,
/// and session mutations (audit finding F4, 2026-05-15).
///
/// Distinct shape from `RateLimiter`:
///   * Keyed on `String` (identity_id / device_id), not `IpAddr`.
///   * Every check counts — there is no separate `record_failure`. The
///     limiter throttles successful operations, not failed auth attempts.
///   * No loopback exemption (the key is per-user, not per-network).
pub struct IdentityActionLimiter {
    entries: DashMap<String, Vec<Instant>>,
    max_per_window: u32,
    window: Duration,
}

impl IdentityActionLimiter {
    pub fn new(max_per_window: u32, window: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            max_per_window,
            window,
        }
    }

    /// Atomically check the cap and record this request. Returns
    /// `Err(retry_after_secs)` when the caller has exceeded their budget;
    /// the caller should respond `429 Too Many Requests`.
    pub fn check_and_record(&self, identity: &str) -> Result<(), u64> {
        let now = Instant::now();
        let window_start = now - self.window;
        let mut entry = self.entries.entry(identity.to_string()).or_default();
        entry.retain(|t| *t > window_start);
        if entry.len() >= self.max_per_window as usize {
            let oldest = entry.iter().min().copied().unwrap_or(now);
            let retry_after = (oldest + self.window)
                .saturating_duration_since(now)
                .as_secs()
                .max(1);
            return Err(retry_after);
        }
        entry.push(now);
        Ok(())
    }

    /// Drop entries whose entire window has elapsed. Called periodically
    /// by the cleanup task spawned at startup.
    pub fn cleanup(&self) {
        let now = Instant::now();
        let window_start = now - self.window;
        self.entries.retain(|_id, attempts| {
            attempts.retain(|t| *t > window_start);
            !attempts.is_empty()
        });
    }
}

pub type SharedActionLimiter = Arc<IdentityActionLimiter>;

/// Spawn a background task that periodically cleans up stale rate limiter entries.
pub fn spawn_cleanup_task<K>(limiter: SharedRateLimiter<K>)
where
    K: RateLimitKey + Send + Sync + 'static,
{
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            limiter.cleanup();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_limiter() -> RateLimiter<IpAddr> {
        RateLimiter::new(RateLimitConfig {
            max_attempts: 3,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(10),
            exempt_loopback: false,
        })
    }

    fn credential_limiter(exempt_loopback: bool) -> RateLimiter<CredentialKey> {
        RateLimiter::new(RateLimitConfig {
            max_attempts: 3,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(10),
            exempt_loopback,
        })
    }

    #[test]
    fn credential_keys_isolate_accounts() {
        // The core guarantee: one account's failures never lock out another,
        // even when both arrive from the same (collapsed) source IP.
        let limiter = credential_limiter(false);
        let ip: IpAddr = "10.0.0.2".parse().unwrap();
        let alice = CredentialKey::new(ip, "alice@example.com");
        let bob = CredentialKey::new(ip, "bob@example.com");

        for _ in 0..3 {
            limiter.record_failure(alice.clone());
        }
        assert!(limiter.check(&alice).is_err());
        assert!(limiter.check(&bob).is_ok());
    }

    #[test]
    fn check_does_not_allocate_for_unknown_key() {
        // An unauthenticated probe of an unseen account must not grow the map;
        // only recorded failures create buckets.
        let limiter = credential_limiter(false);
        let key = CredentialKey::new("10.0.0.2".parse().unwrap(), "ghost@example.com");
        assert!(limiter.check(&key).is_ok());
        assert_eq!(limiter.entries.len(), 0);
    }

    #[test]
    fn credential_key_normalises_email() {
        let ip: IpAddr = "10.0.0.2".parse().unwrap();
        assert_eq!(
            CredentialKey::new(ip, "  Alice@Example.COM "),
            CredentialKey::new(ip, "alice@example.com"),
        );
    }

    #[test]
    fn credential_key_loopback_exempt() {
        let limiter = credential_limiter(true);
        let key = CredentialKey::new("127.0.0.1".parse().unwrap(), "admin@example.com");

        limiter.record_failure(key.clone());
        limiter.record_failure(key.clone());
        limiter.record_failure(key.clone());
        assert!(limiter.check(&key).is_ok());
    }

    #[test]
    fn allows_under_threshold() {
        let limiter = test_limiter();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        limiter.record_failure(ip);
        limiter.record_failure(ip);
        assert!(limiter.check(&ip).is_ok());
    }

    #[test]
    fn blocks_at_threshold() {
        let limiter = test_limiter();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        for _ in 0..3 {
            limiter.record_failure(ip);
        }
        assert!(limiter.check(&ip).is_err());
    }

    #[test]
    fn different_ips_independent() {
        let limiter = test_limiter();
        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();

        for _ in 0..3 {
            limiter.record_failure(ip1);
        }

        assert!(limiter.check(&ip1).is_err());
        assert!(limiter.check(&ip2).is_ok());
    }

    #[test]
    fn loopback_exempt_when_configured() {
        let limiter = RateLimiter::new(RateLimitConfig {
            max_attempts: 1,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(10),
            exempt_loopback: true,
        });
        let ip: IpAddr = "127.0.0.1".parse().unwrap();

        limiter.record_failure(ip);
        limiter.record_failure(ip);
        assert!(limiter.check(&ip).is_ok());
    }

    #[test]
    fn loopback_not_exempt_when_disabled() {
        let limiter = RateLimiter::new(RateLimitConfig {
            max_attempts: 1,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(10),
            exempt_loopback: false,
        });
        let ip: IpAddr = "127.0.0.1".parse().unwrap();

        limiter.record_failure(ip);
        assert!(limiter.check(&ip).is_err());
    }

    #[test]
    fn cleanup_removes_stale_entries() {
        let limiter = RateLimiter::new(RateLimitConfig {
            max_attempts: 10,
            window: Duration::from_millis(1), // Very short window
            lockout: Duration::from_millis(1),
            exempt_loopback: false,
        });
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        limiter.record_failure(ip);
        assert_eq!(limiter.entries.len(), 1);

        std::thread::sleep(Duration::from_millis(5));
        limiter.cleanup();
        assert_eq!(limiter.entries.len(), 0);
    }

    #[test]
    fn returns_retry_after() {
        let limiter = test_limiter();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        for _ in 0..3 {
            limiter.record_failure(ip);
        }

        match limiter.check(&ip) {
            Err(retry_after) => assert!(retry_after > 0),
            Ok(()) => panic!("Expected rate limit"),
        }
    }

    // ── F4: IdentityActionLimiter ──────────────────────────────────────

    fn action_limiter(max: u32, window_secs: u64) -> IdentityActionLimiter {
        IdentityActionLimiter::new(max, Duration::from_secs(window_secs))
    }

    #[test]
    fn action_limiter_allows_under_cap() {
        let lim = action_limiter(3, 60);
        assert!(lim.check_and_record("alice").is_ok());
        assert!(lim.check_and_record("alice").is_ok());
        assert!(lim.check_and_record("alice").is_ok());
    }

    #[test]
    fn action_limiter_rejects_at_cap() {
        let lim = action_limiter(2, 60);
        assert!(lim.check_and_record("alice").is_ok());
        assert!(lim.check_and_record("alice").is_ok());
        match lim.check_and_record("alice") {
            Err(retry_after) => assert!(retry_after > 0),
            Ok(()) => panic!("Expected rate limit"),
        }
    }

    #[test]
    fn action_limiter_isolates_identities() {
        let lim = action_limiter(1, 60);
        assert!(lim.check_and_record("alice").is_ok());
        // Bob has his own bucket even though alice exhausted hers.
        assert!(lim.check_and_record("bob").is_ok());
        assert!(lim.check_and_record("alice").is_err());
        assert!(lim.check_and_record("bob").is_err());
    }

    #[test]
    fn action_limiter_cleanup_drops_empty_buckets() {
        let lim = action_limiter(10, 0); // window=0 → everything is stale
        lim.check_and_record("alice").ok();
        std::thread::sleep(Duration::from_millis(2));
        lim.cleanup();
        assert!(lim.entries.is_empty());
    }
}
