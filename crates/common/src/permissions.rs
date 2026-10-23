//! File permission auditing for config and data directories.
//!
//! Checks that sensitive files have restrictive permissions to prevent
//! unauthorized access to configuration, secrets, and agent data.

use std::path::Path;

use tracing::warn;

/// Result of a permission audit check.
#[derive(Debug)]
pub struct PermissionCheck {
    pub path: String,
    pub severity: PermSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermSeverity {
    Ok,
    Warning,
    Critical,
}

/// Audit file permissions for security issues.
///
/// Checks:
/// - Config files should be mode 600 (owner read/write only)
/// - Data directories should be mode 700 (owner read/write/execute only)
/// - Symlink detection on critical paths
pub fn audit_permissions(config_path: &Path, data_dir: &Path) -> Vec<PermissionCheck> {
    let mut checks = Vec::new();

    // Config file check
    if config_path.exists() {
        check_file_permissions(config_path, 0o600, &mut checks);
        check_symlink(config_path, &mut checks);
    }

    // Data directory check
    if data_dir.exists() {
        check_dir_permissions(data_dir, 0o700, &mut checks);
        check_symlink(data_dir, &mut checks);
    }

    // Check for world-readable secrets in data dir
    let secrets_dir = data_dir.join("secrets");
    if secrets_dir.exists() {
        check_dir_permissions(&secrets_dir, 0o700, &mut checks);
    }

    checks
}

#[cfg(unix)]
fn check_file_permissions(path: &Path, expected: u32, checks: &mut Vec<PermissionCheck>) {
    use std::os::unix::fs::PermissionsExt;

    match std::fs::metadata(path) {
        Ok(meta) => {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                let severity = if mode & 0o007 != 0 {
                    PermSeverity::Critical // World-accessible
                } else {
                    PermSeverity::Warning // Group-accessible
                };
                let msg = format!(
                    "File {} has permissions {:o}, expected {:o}",
                    path.display(),
                    mode,
                    expected
                );
                warn!("{}", msg);
                checks.push(PermissionCheck {
                    path: path.display().to_string(),
                    severity,
                    message: msg,
                });
            } else {
                checks.push(PermissionCheck {
                    path: path.display().to_string(),
                    severity: PermSeverity::Ok,
                    message: format!("File permissions {:o} OK", mode),
                });
            }
        }
        Err(e) => {
            checks.push(PermissionCheck {
                path: path.display().to_string(),
                severity: PermSeverity::Warning,
                message: format!("Cannot read file metadata: {}", e),
            });
        }
    }
}

#[cfg(unix)]
fn check_dir_permissions(path: &Path, expected: u32, checks: &mut Vec<PermissionCheck>) {
    use std::os::unix::fs::PermissionsExt;

    match std::fs::metadata(path) {
        Ok(meta) => {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                let severity = if mode & 0o002 != 0 {
                    PermSeverity::Critical // World-writable
                } else {
                    PermSeverity::Warning // Group-accessible
                };
                let msg = format!(
                    "Directory {} has permissions {:o}, expected {:o}",
                    path.display(),
                    mode,
                    expected
                );
                warn!("{}", msg);
                checks.push(PermissionCheck {
                    path: path.display().to_string(),
                    severity,
                    message: msg,
                });
            } else {
                checks.push(PermissionCheck {
                    path: path.display().to_string(),
                    severity: PermSeverity::Ok,
                    message: format!("Directory permissions {:o} OK", mode),
                });
            }
        }
        Err(e) => {
            checks.push(PermissionCheck {
                path: path.display().to_string(),
                severity: PermSeverity::Warning,
                message: format!("Cannot read directory metadata: {}", e),
            });
        }
    }
}

#[cfg(not(unix))]
fn check_file_permissions(_path: &Path, _expected: u32, checks: &mut Vec<PermissionCheck>) {
    checks.push(PermissionCheck {
        path: _path.display().to_string(),
        severity: PermSeverity::Ok,
        message: "Permission checks not available on this platform".to_string(),
    });
}

#[cfg(not(unix))]
fn check_dir_permissions(_path: &Path, _expected: u32, checks: &mut Vec<PermissionCheck>) {
    checks.push(PermissionCheck {
        path: _path.display().to_string(),
        severity: PermSeverity::Ok,
        message: "Permission checks not available on this platform".to_string(),
    });
}

fn check_symlink(path: &Path, checks: &mut Vec<PermissionCheck>) {
    if path.is_symlink() {
        let msg = format!(
            "{} is a symlink — verify it points to a trusted location",
            path.display()
        );
        warn!("{}", msg);
        checks.push(PermissionCheck {
            path: path.display().to_string(),
            severity: PermSeverity::Warning,
            message: msg,
        });
    }
}

/// Log permission audit results at startup.
pub fn audit_and_log(config_path: &Path, data_dir: &Path) {
    let checks = audit_permissions(config_path, data_dir);

    let critical = checks
        .iter()
        .filter(|c| c.severity == PermSeverity::Critical)
        .count();
    let warnings = checks
        .iter()
        .filter(|c| c.severity == PermSeverity::Warning)
        .count();

    if critical > 0 {
        warn!(
            critical,
            warnings, "Security: file permission issues detected — review and fix"
        );
    } else if warnings > 0 {
        warn!(warnings, "Security: file permission warnings detected");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_nonexistent_paths() {
        let checks = audit_permissions(
            Path::new("/nonexistent/config.toml"),
            Path::new("/nonexistent/data"),
        );
        // Should return empty when paths don't exist
        assert!(checks.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn audit_temp_config_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let checks = audit_permissions(tmp.path(), Path::new("/nonexistent"));
        // Should have at least one check for the config file
        assert!(!checks.is_empty());
    }

    #[test]
    fn symlink_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::write(&target, "test").unwrap();

        // Check that non-symlinks pass
        let mut checks = Vec::new();
        check_symlink(&target, &mut checks);
        assert!(checks.is_empty() || checks.iter().all(|c| c.severity == PermSeverity::Ok));
    }
}
