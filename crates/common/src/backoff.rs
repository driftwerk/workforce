//! The wait before a failed piece of work is tried again, shared by the
//! keepers that retry work noted in Postgres (the artifact index's keeper,
//! memories' mirror applier).

use std::time::Duration;

/// How long `key`, tried `tries` times, waits before its next try: `base`,
/// doubled each time, up to `cap`; and a fifth more or less, by the key and
/// the try, so that those that failed together are not all due together.
/// The same for the same key and try, wherever it is worked out.
pub fn jittered(base: Duration, cap: Duration, key: &str, tries: i32) -> Duration {
    let doublings = u32::try_from(tries.max(1) - 1).unwrap_or(0).min(30);
    let wait = base.saturating_mul(2u32.saturating_pow(doublings)).min(cap);
    // FNV-1a of the key and the try, as a part of a thousand.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in key.bytes().chain(tries.to_le_bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    let part = (hash >> 20) % 401;
    wait.mul_f64(0.8 + part as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_to_its_cap_within_a_fifth() {
        let (base, cap) = (Duration::from_secs(2), Duration::from_secs(60));
        for (tries, plain) in [(1, 2.0), (2, 4.0), (3, 8.0), (10, 60.0)] {
            let wait = jittered(base, cap, "m", tries).as_secs_f64();
            assert!(
                wait >= plain * 0.8 && wait <= plain * 1.2,
                "{tries}: {wait}"
            );
        }
        assert_eq!(jittered(base, cap, "m", 3), jittered(base, cap, "m", 3));
    }
}
