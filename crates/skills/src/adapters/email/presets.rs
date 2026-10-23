//! Provider preset database for common email providers.
//!
//! Compile-time lookup of IMAP/SMTP server details for well-known
//! email providers. No external dependencies — everything is static.

/// IMAP/SMTP server preset for a known email provider.
#[derive(Debug, Clone)]
pub struct ProviderPreset {
    /// Human-readable provider name.
    pub name: &'static str,
    /// IMAP server hostname.
    pub imap_host: &'static str,
    /// IMAP server port.
    pub imap_port: u16,
    /// SMTP server hostname.
    pub smtp_host: &'static str,
    /// SMTP server port.
    pub smtp_port: u16,
    /// Whether to use TLS (true for all except ProtonMail Bridge on localhost).
    pub tls: bool,
    /// Short setup note for the provider.
    pub setup_notes: &'static str,
}

/// Well-known email providers with preset server configurations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownProvider {
    Gmail,
    ICloud,
    Outlook,
    ProtonMail,
    Yahoo,
    Fastmail,
    Custom,
}

impl KnownProvider {
    /// All known providers (excluding Custom).
    pub fn all() -> &'static [KnownProvider] {
        &[
            KnownProvider::Gmail,
            KnownProvider::ICloud,
            KnownProvider::Outlook,
            KnownProvider::ProtonMail,
            KnownProvider::Yahoo,
            KnownProvider::Fastmail,
        ]
    }

    /// Get the server preset for this provider.
    ///
    /// Panics for `Custom` — use `from_str` to resolve provider names,
    /// and check for `Custom` before calling this.
    pub fn preset(&self) -> ProviderPreset {
        match self {
            KnownProvider::Gmail => ProviderPreset {
                name: "Gmail",
                imap_host: "imap.gmail.com",
                imap_port: 993,
                smtp_host: "smtp.gmail.com",
                smtp_port: 587,
                tls: true,
                setup_notes: "Auto-filled Gmail servers",
            },
            KnownProvider::ICloud => ProviderPreset {
                name: "iCloud",
                imap_host: "imap.mail.me.com",
                imap_port: 993,
                smtp_host: "smtp.mail.me.com",
                smtp_port: 587,
                tls: true,
                setup_notes: "Auto-filled iCloud servers",
            },
            KnownProvider::Outlook => ProviderPreset {
                name: "Outlook",
                imap_host: "outlook.office365.com",
                imap_port: 993,
                smtp_host: "smtp.office365.com",
                smtp_port: 587,
                tls: true,
                setup_notes: "Auto-filled Outlook/Microsoft 365 servers",
            },
            KnownProvider::ProtonMail => ProviderPreset {
                name: "ProtonMail",
                imap_host: "127.0.0.1",
                imap_port: 1143,
                smtp_host: "127.0.0.1",
                smtp_port: 1025,
                tls: false,
                setup_notes: "ProtonMail Bridge (localhost)",
            },
            KnownProvider::Yahoo => ProviderPreset {
                name: "Yahoo",
                imap_host: "imap.mail.yahoo.com",
                imap_port: 993,
                smtp_host: "smtp.mail.yahoo.com",
                smtp_port: 587,
                tls: true,
                setup_notes: "Auto-filled Yahoo servers",
            },
            KnownProvider::Fastmail => ProviderPreset {
                name: "Fastmail",
                imap_host: "imap.fastmail.com",
                imap_port: 993,
                smtp_host: "smtp.fastmail.com",
                smtp_port: 587,
                tls: true,
                setup_notes: "Auto-filled Fastmail servers",
            },
            KnownProvider::Custom => {
                panic!("Custom provider has no preset — caller must supply server details")
            }
        }
    }

    /// Provider-specific setup guidance.
    pub fn setup_guidance(&self) -> &'static str {
        match self {
            KnownProvider::Gmail => {
                "Gmail requires an App Password (not your regular password). \
                 Generate one at https://myaccount.google.com/apppasswords — \
                 you need 2-Step Verification enabled first."
            }
            KnownProvider::ICloud => {
                "iCloud requires an app-specific password. \
                 Generate one at https://appleid.apple.com → \
                 Sign-In and Security → App-Specific Passwords."
            }
            KnownProvider::Outlook => {
                "Use your Microsoft account password or an app password. \
                 If your account has MFA enabled, generate an app password \
                 at https://account.live.com/proofs/AppPassword"
            }
            KnownProvider::ProtonMail => {
                "ProtonMail requires ProtonMail Bridge running locally. \
                 Download Bridge from https://proton.me/mail/bridge — \
                 it creates a local IMAP/SMTP server on 127.0.0.1. \
                 Use the Bridge-generated password, not your ProtonMail password."
            }
            KnownProvider::Yahoo => {
                "Yahoo requires an App Password. \
                 Generate one at https://login.yahoo.com/account/security → \
                 Generate app password."
            }
            KnownProvider::Fastmail => {
                "Fastmail supports app passwords for enhanced security. \
                 Generate one at Settings → Privacy & Security → \
                 Integrations → New App Password. Select IMAP/SMTP access."
            }
            KnownProvider::Custom => {
                "For custom email providers, you'll need to supply the IMAP and SMTP \
                 server hostnames and ports manually. Check your provider's documentation \
                 for the correct settings."
            }
        }
    }

    /// Try to parse a provider name (case-insensitive).
    ///
    /// Returns `Custom` for unrecognised names.
    pub fn from_name(name: &str) -> KnownProvider {
        match name.to_lowercase().as_str() {
            "gmail" | "google" => KnownProvider::Gmail,
            "icloud" | "apple" | "me.com" => KnownProvider::ICloud,
            "outlook" | "microsoft" | "hotmail" | "live" | "office365" => KnownProvider::Outlook,
            "protonmail" | "proton" => KnownProvider::ProtonMail,
            "yahoo" => KnownProvider::Yahoo,
            "fastmail" => KnownProvider::Fastmail,
            "custom" => KnownProvider::Custom,
            _ => KnownProvider::Custom,
        }
    }

    /// Returns a serialisable name for this provider.
    pub fn as_str(&self) -> &'static str {
        match self {
            KnownProvider::Gmail => "gmail",
            KnownProvider::ICloud => "icloud",
            KnownProvider::Outlook => "outlook",
            KnownProvider::ProtonMail => "protonmail",
            KnownProvider::Yahoo => "yahoo",
            KnownProvider::Fastmail => "fastmail",
            KnownProvider::Custom => "custom",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_providers_returns_six() {
        assert_eq!(KnownProvider::all().len(), 6);
        // Custom should NOT be in the list
        assert!(!KnownProvider::all().contains(&KnownProvider::Custom));
    }

    #[test]
    fn preset_gmail() {
        let p = KnownProvider::Gmail.preset();
        assert_eq!(p.name, "Gmail");
        assert_eq!(p.imap_host, "imap.gmail.com");
        assert_eq!(p.imap_port, 993);
        assert_eq!(p.smtp_host, "smtp.gmail.com");
        assert_eq!(p.smtp_port, 587);
        assert!(p.tls);
    }

    #[test]
    fn preset_icloud() {
        let p = KnownProvider::ICloud.preset();
        assert_eq!(p.imap_host, "imap.mail.me.com");
        assert_eq!(p.smtp_host, "smtp.mail.me.com");
        assert!(p.tls);
    }

    #[test]
    fn preset_outlook() {
        let p = KnownProvider::Outlook.preset();
        assert_eq!(p.imap_host, "outlook.office365.com");
        assert_eq!(p.smtp_host, "smtp.office365.com");
        assert!(p.tls);
    }

    #[test]
    fn preset_protonmail() {
        let p = KnownProvider::ProtonMail.preset();
        assert_eq!(p.imap_host, "127.0.0.1");
        assert_eq!(p.imap_port, 1143);
        assert_eq!(p.smtp_host, "127.0.0.1");
        assert_eq!(p.smtp_port, 1025);
        assert!(!p.tls); // ProtonMail Bridge uses plaintext on localhost
    }

    #[test]
    fn preset_yahoo() {
        let p = KnownProvider::Yahoo.preset();
        assert_eq!(p.imap_host, "imap.mail.yahoo.com");
        assert_eq!(p.smtp_host, "smtp.mail.yahoo.com");
    }

    #[test]
    fn preset_fastmail() {
        let p = KnownProvider::Fastmail.preset();
        assert_eq!(p.imap_host, "imap.fastmail.com");
        assert_eq!(p.smtp_host, "smtp.fastmail.com");
    }

    #[test]
    #[should_panic(expected = "Custom provider has no preset")]
    fn preset_custom_panics() {
        KnownProvider::Custom.preset();
    }

    #[test]
    fn from_name_case_insensitive() {
        assert_eq!(KnownProvider::from_name("gmail"), KnownProvider::Gmail);
        assert_eq!(KnownProvider::from_name("Gmail"), KnownProvider::Gmail);
        assert_eq!(KnownProvider::from_name("GMAIL"), KnownProvider::Gmail);
        assert_eq!(KnownProvider::from_name("Google"), KnownProvider::Gmail);
    }

    #[test]
    fn from_name_aliases() {
        assert_eq!(KnownProvider::from_name("icloud"), KnownProvider::ICloud);
        assert_eq!(KnownProvider::from_name("apple"), KnownProvider::ICloud);
        assert_eq!(KnownProvider::from_name("me.com"), KnownProvider::ICloud);
        assert_eq!(KnownProvider::from_name("outlook"), KnownProvider::Outlook);
        assert_eq!(
            KnownProvider::from_name("microsoft"),
            KnownProvider::Outlook
        );
        assert_eq!(KnownProvider::from_name("hotmail"), KnownProvider::Outlook);
        assert_eq!(KnownProvider::from_name("live"), KnownProvider::Outlook);
        assert_eq!(
            KnownProvider::from_name("office365"),
            KnownProvider::Outlook
        );
        assert_eq!(
            KnownProvider::from_name("protonmail"),
            KnownProvider::ProtonMail
        );
        assert_eq!(
            KnownProvider::from_name("proton"),
            KnownProvider::ProtonMail
        );
        assert_eq!(KnownProvider::from_name("yahoo"), KnownProvider::Yahoo);
        assert_eq!(
            KnownProvider::from_name("fastmail"),
            KnownProvider::Fastmail
        );
    }

    #[test]
    fn from_name_unknown_returns_custom() {
        assert_eq!(
            KnownProvider::from_name("unknown-provider"),
            KnownProvider::Custom
        );
        assert_eq!(KnownProvider::from_name(""), KnownProvider::Custom);
        assert_eq!(
            KnownProvider::from_name("my-company.com"),
            KnownProvider::Custom
        );
    }

    #[test]
    fn guidance_text_not_empty() {
        for provider in KnownProvider::all() {
            let guidance = provider.setup_guidance();
            assert!(!guidance.is_empty(), "{:?} has empty guidance", provider);
        }
        // Custom too
        assert!(!KnownProvider::Custom.setup_guidance().is_empty());
    }

    #[test]
    fn guidance_gmail_mentions_app_password() {
        let g = KnownProvider::Gmail.setup_guidance();
        assert!(g.contains("App Password"), "Gmail guidance: {g}");
        assert!(g.contains("myaccount.google.com"), "Gmail guidance: {g}");
    }

    #[test]
    fn guidance_icloud_mentions_app_specific_password() {
        let g = KnownProvider::ICloud.setup_guidance();
        assert!(g.contains("app-specific password"), "iCloud guidance: {g}");
        assert!(g.contains("appleid.apple.com"), "iCloud guidance: {g}");
    }

    #[test]
    fn guidance_protonmail_mentions_bridge() {
        let g = KnownProvider::ProtonMail.setup_guidance();
        assert!(g.contains("Bridge"), "ProtonMail guidance: {g}");
    }

    #[test]
    fn as_str_roundtrip() {
        for provider in KnownProvider::all() {
            let s = provider.as_str();
            assert_eq!(KnownProvider::from_name(s), *provider);
        }
    }
}
