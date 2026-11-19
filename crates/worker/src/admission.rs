//! Whether there is room for one more scope: a hard ceiling per weight, and,
//! when `[worker.admission]` is set, the host's memory and CPU headroom.
//!
//! Memory is checked two ways. What is *committed* (a reserve per live or
//! in-flight scope) stops a burst of seats whose CLIs have not started yet
//! from all fitting the same free memory; what is *measured* (the host's
//! `MemAvailable`) sees what reserves cannot, such as a light scope whose
//! turn started a database and a test run. Either short, the scope waits.

use std::path::Path;

use eyre::{Context, Result};
use serde::Serialize;

use crate::config::{AdmissionConfig, WorkerConfig};
use crate::weight::Weight;

const GIB: u64 = 1024 * 1024 * 1024;

/// The host's memory and CPU, as admission reads them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    /// The share of the last ten seconds some task waited for a CPU, in
    /// percent (PSI `cpu some avg10`); `None` where the kernel has no PSI.
    pub cpu_pressure: Option<f64>,
}

/// Reads the host's load.
pub trait HostLoad: Send + Sync {
    fn read(&self) -> Result<Reading>;
}

/// The real host: `/proc/meminfo` and `/proc/pressure/cpu`.
pub struct ProcLoad;

impl HostLoad for ProcLoad {
    fn read(&self) -> Result<Reading> {
        let meminfo = std::fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
        let (memory_total_bytes, memory_available_bytes) = parse_meminfo(&meminfo)?;
        let cpu_pressure = std::fs::read_to_string(Path::new("/proc/pressure/cpu"))
            .ok()
            .and_then(|text| parse_pressure(&text));
        Ok(Reading {
            memory_total_bytes,
            memory_available_bytes,
            cpu_pressure,
        })
    }
}

/// `MemTotal` and `MemAvailable` from `/proc/meminfo`, in bytes.
pub(crate) fn parse_meminfo(text: &str) -> Result<(u64, u64)> {
    let field = |name: &str| -> Result<u64> {
        let kib = text
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|rest| rest.split_whitespace().next())
            .ok_or_else(|| eyre::eyre!("/proc/meminfo has no {name}"))?
            .parse::<u64>()
            .with_context(|| format!("/proc/meminfo {name}"))?;
        Ok(kib * 1024)
    };
    Ok((field("MemTotal")?, field("MemAvailable")?))
}

/// `some avg10` from a PSI file.
pub(crate) fn parse_pressure(text: &str) -> Option<f64> {
    text.lines()
        .find_map(|line| line.strip_prefix("some "))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
}

/// The scopes a decision counts: live ones and those being prepared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Census {
    pub full: u32,
    pub light: u32,
}

impl Census {
    pub fn add(&mut self, weight: Weight) {
        match weight {
            Weight::Full => self.full += 1,
            Weight::Light => self.light += 1,
        }
    }
}

/// Why a scope was not admitted, for the log; the hub hears `AtCapacity`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shortfall {
    /// As many scopes of its weight as the worker takes.
    Ceiling { held: u32, ceiling: u32 },
    /// What admitted scopes reserve leaves no room for its reserve.
    Committed { reserved: u64, budget: u64 },
    /// The host's free memory less its reserve would fall under the floor.
    Available { available: u64, floor: u64 },
    /// The CPUs are contended past the configured pressure.
    Cpu { pressure: f64, max: f64 },
}

impl std::fmt::Display for Shortfall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ceiling { held, ceiling } => {
                write!(f, "{held} of {ceiling} scopes of its weight held")
            }
            Self::Committed { reserved, budget } => write!(
                f,
                "{:.1} GiB reserved of a {:.1} GiB budget",
                *reserved as f64 / GIB as f64,
                *budget as f64 / GIB as f64
            ),
            Self::Available { available, floor } => write!(
                f,
                "{:.1} GiB available, floor {:.1} GiB",
                *available as f64 / GIB as f64,
                *floor as f64 / GIB as f64
            ),
            Self::Cpu { pressure, max } => write!(f, "CPU pressure {pressure:.0}% over {max:.0}%"),
        }
    }
}

/// The most scopes of `weight` at once. With no light ceiling every scope
/// counts against `max_scopes`, as before scopes had weights.
fn ceiling(config: &WorkerConfig, census: &Census, weight: Weight) -> (u32, u32) {
    match (config.max_light_scopes, weight) {
        (0, _) => (census.full + census.light, config.max_scopes),
        (_, Weight::Full) => (census.full, config.max_scopes),
        (light, Weight::Light) => (census.light, light),
    }
}

/// What admitting a scope of `weight` reserves, in bytes.
fn reserve(admission: &AdmissionConfig, weight: Weight) -> u64 {
    u64::from(match weight {
        Weight::Full => admission.full_reserve_gib,
        Weight::Light => admission.light_reserve_gib,
    }) * GIB
}

/// Whether one more scope of `weight` fits beside `census`, given the host's
/// `reading` when one could be taken. A reading that could not be taken
/// leaves the ceilings to decide.
pub fn decide(
    config: &WorkerConfig,
    census: &Census,
    weight: Weight,
    reading: Option<&Reading>,
) -> Result<(), Shortfall> {
    let (held, ceiling) = ceiling(config, census, weight);
    if held >= ceiling {
        return Err(Shortfall::Ceiling { held, ceiling });
    }
    let (Some(admission), Some(reading)) = (&config.admission, reading) else {
        return Ok(());
    };
    let floor = u64::from(admission.memory_floor_gib) * GIB;
    let wanted = reserve(admission, weight);
    let reserved = u64::from(census.full) * reserve(admission, Weight::Full)
        + u64::from(census.light) * reserve(admission, Weight::Light);
    let budget = reading.memory_total_bytes.saturating_sub(floor);
    if reserved + wanted > budget {
        return Err(Shortfall::Committed { reserved, budget });
    }
    if reading.memory_available_bytes.saturating_sub(wanted) < floor {
        return Err(Shortfall::Available {
            available: reading.memory_available_bytes,
            floor,
        });
    }
    if let (Some(max), Some(pressure)) = (admission.cpu_pressure_max, reading.cpu_pressure) {
        if pressure > max {
            return Err(Shortfall::Cpu { pressure, max });
        }
    }
    Ok(())
}

/// How many more light scopes would be admitted now: the scaling signal.
pub fn light_seats_free(config: &WorkerConfig, census: &Census, reading: Option<&Reading>) -> u32 {
    let mut census = *census;
    let mut free = 0;
    // Bounded by the ceiling, so at most a few dozen rounds.
    while decide(config, &census, Weight::Light, reading).is_ok() {
        census.add(Weight::Light);
        free += 1;
        if let (Some(admission), Some(reading)) = (&config.admission, reading) {
            // Measured headroom does not shrink as the census grows, so
            // charge each seat counted against it too.
            let spent = u64::from(free) * reserve(admission, Weight::Light);
            let floor = u64::from(admission.memory_floor_gib) * GIB;
            if reading
                .memory_available_bytes
                .saturating_sub(spent + reserve(admission, Weight::Light))
                < floor
            {
                break;
            }
        }
    }
    free
}

/// What the daemon publishes for the host's metrics after each decision,
/// release and sweep, at `state_dir/load.json`.
#[derive(Debug, Clone, Serialize)]
pub struct Load {
    pub at: chrono::DateTime<chrono::Utc>,
    pub full: u32,
    pub light: u32,
    pub max_full: u32,
    /// 0 when light scopes count against `max_full`.
    pub max_light: u32,
    pub reserved_bytes: u64,
    pub memory_total_bytes: Option<u64>,
    pub memory_available_bytes: Option<u64>,
    pub cpu_pressure: Option<f64>,
    pub light_seats_free: u32,
    /// Prepares refused for room since the daemon started.
    pub refusals: u64,
}

impl Load {
    pub fn of(
        config: &WorkerConfig,
        census: &Census,
        reading: Option<&Reading>,
        refusals: u64,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let reserved_bytes = config.admission.as_ref().map_or(0, |admission| {
            u64::from(census.full) * reserve(admission, Weight::Full)
                + u64::from(census.light) * reserve(admission, Weight::Light)
        });
        Self {
            at,
            full: census.full,
            light: census.light,
            max_full: config.max_scopes,
            max_light: config.max_light_scopes,
            reserved_bytes,
            memory_total_bytes: reading.map(|r| r.memory_total_bytes),
            memory_available_bytes: reading.map(|r| r.memory_available_bytes),
            cpu_pressure: reading.and_then(|r| r.cpu_pressure),
            light_seats_free: light_seats_free(config, census, reading),
            refusals,
        }
    }
}

/// Where the daemon writes its [`Load`].
pub fn load_path(config: &WorkerConfig) -> std::path::PathBuf {
    config.state_dir.join("load.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::sample;

    fn config(admission: Option<AdmissionConfig>) -> WorkerConfig {
        WorkerConfig {
            max_scopes: 4,
            max_light_scopes: 10,
            admission,
            ..sample()
        }
    }

    fn admission() -> AdmissionConfig {
        AdmissionConfig {
            full_reserve_gib: 3,
            light_reserve_gib: 1,
            memory_floor_gib: 6,
            cpu_pressure_max: Some(80.0),
        }
    }

    fn reading(total_gib: u64, available_gib: u64) -> Reading {
        Reading {
            memory_total_bytes: total_gib * GIB,
            memory_available_bytes: available_gib * GIB,
            cpu_pressure: Some(5.0),
        }
    }

    fn census(full: u32, light: u32) -> Census {
        Census { full, light }
    }

    #[test]
    fn without_a_light_ceiling_every_scope_counts_against_max_scopes() {
        let config = WorkerConfig {
            max_light_scopes: 0,
            ..config(None)
        };
        assert!(decide(&config, &census(2, 1), Weight::Light, None).is_ok());
        assert_eq!(
            decide(&config, &census(2, 2), Weight::Light, None),
            Err(Shortfall::Ceiling {
                held: 4,
                ceiling: 4
            })
        );
        assert!(decide(&config, &census(2, 2), Weight::Full, None).is_err());
    }

    #[test]
    fn each_weight_has_its_own_ceiling() {
        let config = config(None);
        assert!(decide(&config, &census(4, 0), Weight::Light, None).is_ok());
        assert_eq!(
            decide(&config, &census(4, 0), Weight::Full, None),
            Err(Shortfall::Ceiling {
                held: 4,
                ceiling: 4
            })
        );
        assert!(decide(&config, &census(0, 9), Weight::Light, None).is_ok());
        assert!(decide(&config, &census(0, 10), Weight::Light, None).is_err());
        assert!(decide(&config, &census(0, 10), Weight::Full, None).is_ok());
    }

    #[test]
    fn committed_reserves_cap_a_burst_the_host_has_not_felt_yet() {
        let config = config(Some(admission()));
        // 16 GiB host, floor 6: a 10 GiB budget is 2 full (6) + 4 light (4).
        let fresh = reading(16, 15);
        assert!(decide(&config, &census(2, 3), Weight::Light, Some(&fresh)).is_ok());
        assert!(matches!(
            decide(&config, &census(2, 4), Weight::Light, Some(&fresh)),
            Err(Shortfall::Committed { .. })
        ));
        assert!(matches!(
            decide(&config, &census(2, 2), Weight::Full, Some(&fresh)),
            Err(Shortfall::Committed { .. })
        ));
    }

    #[test]
    fn measured_memory_sees_growth_no_reserve_counts() {
        let config = config(Some(admission()));
        // One light scope reserved, but its turn started a database and a
        // test run: 6.5 GiB left, and a 1 GiB reserve would go under 6.
        let grown = Reading {
            memory_available_bytes: 6 * GIB + GIB / 2,
            ..reading(64, 0)
        };
        assert!(matches!(
            decide(&config, &census(0, 1), Weight::Light, Some(&grown)),
            Err(Shortfall::Available { .. })
        ));
        assert!(decide(&config, &census(0, 1), Weight::Light, Some(&reading(64, 8))).is_ok());
    }

    #[test]
    fn cpu_pressure_over_the_max_refuses_and_a_missing_reading_does_not() {
        let config = config(Some(admission()));
        let busy = Reading {
            cpu_pressure: Some(92.0),
            ..reading(64, 40)
        };
        assert!(matches!(
            decide(&config, &census(0, 0), Weight::Light, Some(&busy)),
            Err(Shortfall::Cpu { .. })
        ));
        let no_psi = Reading {
            cpu_pressure: None,
            ..busy
        };
        assert!(decide(&config, &census(0, 0), Weight::Light, Some(&no_psi)).is_ok());
        assert!(
            decide(&config, &census(0, 0), Weight::Light, None).is_ok(),
            "an unreadable host leaves the ceilings to decide"
        );
    }

    #[test]
    fn light_seats_free_is_the_tightest_of_ceiling_reserve_and_memory() {
        let config = config(Some(admission()));
        assert_eq!(
            light_seats_free(&config, &census(0, 7), Some(&reading(64, 60))),
            3,
            "ceiling"
        );
        assert_eq!(
            light_seats_free(&config, &census(2, 0), Some(&reading(16, 15))),
            4,
            "committed"
        );
        assert_eq!(
            light_seats_free(&config, &census(0, 0), Some(&reading(64, 9))),
            3,
            "measured: 9 GiB less 3 reserves leaves the 6 GiB floor"
        );
        assert_eq!(light_seats_free(&config, &census(0, 0), None), 10);
    }

    #[test]
    fn meminfo_and_pressure_parse_as_the_kernel_writes_them() {
        let meminfo =
            "MemTotal:       63393836 kB\nMemFree:         1234 kB\nMemAvailable:   51234567 kB\n";
        assert_eq!(
            parse_meminfo(meminfo).unwrap(),
            (63_393_836 * 1024, 51_234_567 * 1024)
        );
        assert!(parse_meminfo("MemTotal: 1 kB\n").is_err());
        let psi = "some avg10=12.34 avg60=1.00 avg300=0.50 total=123\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(parse_pressure(psi), Some(12.34));
        assert_eq!(parse_pressure("garbage"), None);
    }
}
