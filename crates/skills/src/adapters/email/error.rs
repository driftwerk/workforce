//! Email skill error types.
//!
//! Provides clear, actionable error messages for IMAP connection failures
//! without leaking credentials in logs or error output.

use std::fmt;
use std::time::Duration;

/// Email skill errors — designed to be actionable without exposing secrets.
#[derive(Debug)]
pub enum EmailError {
    /// TCP connection to the IMAP server was refused.
    ConnectionRefused {
        /// IMAP server hostname.
        host: String,
        /// IMAP server port.
        port: u16,
    },
    /// TLS handshake failed during connection.
    TlsHandshake {
        /// IMAP server hostname.
        host: String,
        /// Underlying error detail (scrubbed of credentials).
        detail: String,
    },
    /// Authentication (LOGIN) was rejected by the server.
    AuthFailed {
        /// Account name (NOT the password).
        account: String,
    },
    /// An operation timed out.
    Timeout {
        /// What operation timed out (e.g. "connect", "login", "noop").
        operation: String,
        /// How long we waited.
        duration: Duration,
    },
    /// The IMAP session was dropped or expired mid-use.
    SessionExpired {
        /// Account name.
        account: String,
    },
    /// The requested account name doesn't exist in the config.
    AccountNotFound {
        /// Account name that was requested.
        name: String,
    },
    /// Catch-all for upstream `async-imap` errors (scrubbed of credentials).
    Imap(String),
    /// TCP connection to the SMTP server was refused.
    SmtpConnectionRefused {
        /// SMTP server hostname.
        host: String,
        /// SMTP server port.
        port: u16,
    },
    /// TLS handshake failed during SMTP connection.
    SmtpTlsHandshake {
        /// SMTP server hostname.
        host: String,
        /// Underlying error detail (scrubbed of credentials).
        detail: String,
    },
    /// SMTP authentication was rejected by the server.
    SmtpAuthFailed {
        /// Account name (NOT the password).
        account: String,
    },
    /// SMTP message send failed.
    SmtpSendFailed {
        /// Error detail (scrubbed of credentials).
        detail: String,
    },
    /// A recipient email address is invalid.
    InvalidRecipient {
        /// The invalid address.
        address: String,
    },
    /// A target folder does not exist on the server.
    FolderNotFound {
        /// The folder name that was not found.
        folder: String,
        /// Available folder names for the account.
        available: Vec<String>,
    },
    /// A message move/copy operation failed.
    MoveFailure {
        /// Error detail.
        detail: String,
    },
}

impl fmt::Display for EmailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionRefused { host, port } => {
                write!(
                    f,
                    "Connection refused to {host}:{port} — check host and port"
                )
            }
            Self::TlsHandshake { host, detail } => {
                write!(
                    f,
                    "TLS handshake failed for {host} — the server may not support TLS on this port ({detail})"
                )
            }
            Self::AuthFailed { account } => {
                write!(
                    f,
                    "Authentication failed for account '{account}' — check username and password"
                )
            }
            Self::Timeout {
                operation,
                duration,
            } => {
                write!(
                    f,
                    "Timeout during {operation} after {:.0}s — server may be unreachable",
                    duration.as_secs_f64()
                )
            }
            Self::SessionExpired { account } => {
                write!(
                    f,
                    "IMAP session expired for account '{account}' — reconnecting"
                )
            }
            Self::AccountNotFound { name } => {
                write!(
                    f,
                    "Email account '{name}' not found — check configured account names"
                )
            }
            Self::Imap(msg) => {
                write!(f, "IMAP error: {msg}")
            }
            Self::SmtpConnectionRefused { host, port } => {
                write!(
                    f,
                    "SMTP connection refused to {host}:{port} — check SMTP host and port"
                )
            }
            Self::SmtpTlsHandshake { host, detail } => {
                write!(f, "SMTP TLS handshake failed for {host} — {detail}")
            }
            Self::SmtpAuthFailed { account } => {
                write!(
                    f,
                    "SMTP authentication failed for account '{account}' — check username and password"
                )
            }
            Self::SmtpSendFailed { detail } => {
                write!(f, "Failed to send email: {detail}")
            }
            Self::InvalidRecipient { address } => {
                write!(
                    f,
                    "Invalid recipient address: '{address}' — check the email format"
                )
            }
            Self::FolderNotFound { folder, available } => {
                write!(
                    f,
                    "Folder '{}' not found. Available folders: {}",
                    folder,
                    available.join(", ")
                )
            }
            Self::MoveFailure { detail } => {
                write!(f, "Failed to move messages: {detail}")
            }
        }
    }
}

impl std::error::Error for EmailError {}

/// Convert an `async_imap::error::Error` into our `EmailError`.
#[cfg(feature = "email")]
#[allow(dead_code)] // Will be used by tools in DW-381+
pub(crate) fn from_imap_error(err: async_imap::error::Error) -> EmailError {
    EmailError::Imap(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_never_contain_passwords() {
        let password = "super_secret_p@ssw0rd_123!";

        let errors = vec![
            EmailError::ConnectionRefused {
                host: "imap.example.com".into(),
                port: 993,
            },
            EmailError::TlsHandshake {
                host: "imap.example.com".into(),
                detail: "certificate verify failed".into(),
            },
            EmailError::AuthFailed {
                account: "work".into(),
            },
            EmailError::Timeout {
                operation: "connect".into(),
                duration: Duration::from_secs(30),
            },
            EmailError::SessionExpired {
                account: "personal".into(),
            },
            EmailError::AccountNotFound {
                name: "nonexistent".into(),
            },
            EmailError::Imap("NO [AUTHENTICATIONFAILED] Invalid credentials".into()),
            EmailError::SmtpConnectionRefused {
                host: "smtp.example.com".into(),
                port: 587,
            },
            EmailError::SmtpTlsHandshake {
                host: "smtp.example.com".into(),
                detail: "certificate verify failed".into(),
            },
            EmailError::SmtpAuthFailed {
                account: "work".into(),
            },
            EmailError::SmtpSendFailed {
                detail: "relay access denied".into(),
            },
            EmailError::InvalidRecipient {
                address: "bad@address".into(),
            },
            EmailError::FolderNotFound {
                folder: "NonExistent".into(),
                available: vec!["INBOX".into(), "Sent".into(), "Trash".into()],
            },
            EmailError::MoveFailure {
                detail: "COPY command failed".into(),
            },
        ];

        for err in &errors {
            let msg = err.to_string();
            assert!(
                !msg.contains(password),
                "Error message leaked password: {msg}"
            );
        }
    }

    #[test]
    fn display_connection_refused() {
        let err = EmailError::ConnectionRefused {
            host: "imap.mail.me.com".into(),
            port: 993,
        };
        assert_eq!(
            err.to_string(),
            "Connection refused to imap.mail.me.com:993 — check host and port"
        );
    }

    #[test]
    fn display_tls_handshake() {
        let err = EmailError::TlsHandshake {
            host: "imap.example.com".into(),
            detail: "certificate verify failed".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("TLS handshake failed"));
        assert!(msg.contains("imap.example.com"));
    }

    #[test]
    fn display_auth_failed() {
        let err = EmailError::AuthFailed {
            account: "icloud".into(),
        };
        assert_eq!(
            err.to_string(),
            "Authentication failed for account 'icloud' — check username and password"
        );
    }

    #[test]
    fn display_timeout() {
        let err = EmailError::Timeout {
            operation: "connect".into(),
            duration: Duration::from_secs(30),
        };
        let msg = err.to_string();
        assert!(msg.contains("Timeout during connect"));
        assert!(msg.contains("30s"));
    }

    #[test]
    fn display_account_not_found() {
        let err = EmailError::AccountNotFound {
            name: "nonexistent".into(),
        };
        assert_eq!(
            err.to_string(),
            "Email account 'nonexistent' not found — check configured account names"
        );
    }

    #[test]
    fn into_eyre_report() {
        let err = EmailError::AuthFailed {
            account: "test".into(),
        };
        let report: eyre::Report = eyre::eyre!("{}", err);
        let msg = report.to_string();
        assert!(msg.contains("Authentication failed"));
    }

    #[test]
    fn display_smtp_connection_refused() {
        let err = EmailError::SmtpConnectionRefused {
            host: "smtp.example.com".into(),
            port: 587,
        };
        assert_eq!(
            err.to_string(),
            "SMTP connection refused to smtp.example.com:587 — check SMTP host and port"
        );
    }

    #[test]
    fn display_smtp_tls_handshake() {
        let err = EmailError::SmtpTlsHandshake {
            host: "smtp.example.com".into(),
            detail: "certificate verify failed".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("SMTP TLS handshake failed"));
        assert!(msg.contains("smtp.example.com"));
    }

    #[test]
    fn display_smtp_auth_failed() {
        let err = EmailError::SmtpAuthFailed {
            account: "work".into(),
        };
        assert_eq!(
            err.to_string(),
            "SMTP authentication failed for account 'work' — check username and password"
        );
    }

    #[test]
    fn display_smtp_send_failed() {
        let err = EmailError::SmtpSendFailed {
            detail: "relay access denied".into(),
        };
        assert_eq!(err.to_string(), "Failed to send email: relay access denied");
    }

    #[test]
    fn display_invalid_recipient() {
        let err = EmailError::InvalidRecipient {
            address: "not-an-email".into(),
        };
        assert_eq!(
            err.to_string(),
            "Invalid recipient address: 'not-an-email' — check the email format"
        );
    }

    #[test]
    fn display_folder_not_found() {
        let err = EmailError::FolderNotFound {
            folder: "NonExistent".into(),
            available: vec!["INBOX".into(), "Sent".into(), "Trash".into()],
        };
        assert_eq!(
            err.to_string(),
            "Folder 'NonExistent' not found. Available folders: INBOX, Sent, Trash"
        );
    }

    #[test]
    fn display_move_failure() {
        let err = EmailError::MoveFailure {
            detail: "COPY command failed".into(),
        };
        assert_eq!(
            err.to_string(),
            "Failed to move messages: COPY command failed"
        );
    }
}
