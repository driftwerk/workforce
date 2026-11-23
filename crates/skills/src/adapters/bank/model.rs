//! What a bank connection reads, in the provider's own figures: amounts are
//! passed on as the provider gave them, never computed with.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Serialize;
use serde_json::Number;

/// About how long a bank's approval lasts, an estimate: BankSync never says
/// a consent's end over its API. Its UK page (banksync.io, "UK open banking
/// access lasts 180 days") and its EU banks' ("again at least every 180
/// days", Delegated Regulation (EU) 2022/2360 Art. 10a) say 180 days; its
/// consents guide says UK banks "typically use a 90-day reconfirmation
/// cycle". Only a reminder is drawn from it: a lapse is what BankSync says.
pub const CONSENT_DAYS: i64 = 180;

/// The days before an approval lapses on which the person is reminded.
const REMIND_AT: &[i64] = &[7, 1, 0];

/// The bank one connection reads.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Connection {
    pub bank: String,
    pub connected: bool,
    pub status: Option<String>,
    /// Whether the bank's full history has arrived: `ready`, `syncing`.
    pub history: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Account {
    pub id: String,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    /// The account number's last four digits, as `•••• 1234`.
    pub number: String,
    pub currency: String,
    pub balance: Option<Number>,
    pub available: Option<Number>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Balance {
    pub account: String,
    pub name: Option<String>,
    pub currency: String,
    pub amount: Number,
    pub available: Option<Number>,
    pub pending: Option<Number>,
    pub credit_limit: Option<Number>,
    /// When the bank said it was this, where it says.
    pub as_of: Option<String>,
    /// When the provider last read it from the bank.
    pub retrieved_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Transaction {
    pub id: String,
    pub date: String,
    pub description: String,
    pub merchant: Option<String>,
    /// Money in is positive, out is negative.
    pub amount: Number,
    pub currency: String,
    pub category: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub reference: Option<String>,
    pub pending: bool,
    pub card: Option<String>,
}

impl Transaction {
    /// Whether `needle` (lower-cased) is in what it says.
    pub fn mentions(&self, needle: &str) -> bool {
        [
            Some(&self.description),
            self.merchant.as_ref(),
            self.reference.as_ref(),
            self.category.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|text| text.to_lowercase().contains(needle))
    }
}

/// One page of an account's transactions.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub transactions: Vec<Transaction>,
    pub cursor: Option<String>,
    pub has_more: bool,
    /// The provider gave only part of the window.
    pub partial: bool,
}

/// The days transactions are read for, both inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

impl Window {
    /// `from`..`to`, the last 30 days to `to` when `from` is not given, and
    /// to `today` when `to` is not.
    pub fn of(
        from: Option<NaiveDate>,
        to: Option<NaiveDate>,
        today: NaiveDate,
    ) -> Result<Self, BankError> {
        let to = to.unwrap_or(today);
        let from = from.unwrap_or(to - Duration::days(30));
        match from <= to {
            true => Ok(Self { from, to }),
            false => Err(BankError::Invalid(
                "`from` is after `to`: give the earlier date first".into(),
            )),
        }
    }
}

/// When a connection's approval lapses, by the UTC calendar, and whether to
/// remind the person about it today. The provider doesn't say when, so it is
/// `estimated`: [`CONSENT_DAYS`] from when it was last approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Approval {
    pub until: NaiveDate,
    pub days_left: i64,
    pub remind: bool,
    /// It lapses within the week, or has.
    pub due: bool,
    pub estimated: bool,
}

impl Approval {
    pub fn of(consented_at: DateTime<Utc>, today: NaiveDate) -> Self {
        let until = (consented_at + Duration::days(CONSENT_DAYS)).date_naive();
        let days_left = (until - today).num_days();
        Self {
            until,
            days_left,
            remind: REMIND_AT.contains(&days_left),
            due: days_left <= REMIND_AT[0],
            estimated: true,
        }
    }
}

/// `number` as its last four digits alone.
pub fn masked(number: &str) -> String {
    let digits: Vec<char> = number.chars().filter(char::is_ascii_alphanumeric).collect();
    let tail: String = digits[digits.len().saturating_sub(4)..].iter().collect();
    format!("•••• {tail}")
}

/// Why a read didn't happen, in words the agent can act on. Never the
/// provider's body, an id or an amount: what is logged of it is this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BankError {
    /// The bank's approval lapsed or was withdrawn.
    NeedsReconnect,
    /// The provider no longer accepts the key.
    Revoked,
    /// The key can't read banks.
    ScopeMissing,
    /// The key can do more than read, so it is never used.
    NotReadOnly,
    NotFound(&'static str),
    RateLimited {
        retry_after: u64,
    },
    Invalid(String),
    NotConfigured(&'static str),
    Unavailable(Option<u16>),
}

impl std::fmt::Display for BankError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedsReconnect => f.write_str(
                "The bank's approval has lapsed or needs renewing. The person renews it in BankSync, then presses Reconnect on the Bank connection in Desk.",
            ),
            Self::Revoked => f.write_str(
                "BankSync no longer accepts this connection's key (revoked or expired). Set the connection up again with connections.setup.",
            ),
            Self::ScopeMissing => f.write_str(
                "This connection's key can't read banks. Make a key with banks:read and institutions:read only, and set the connection up again.",
            ),
            Self::NotReadOnly => f.write_str(
                "This connection's key can do more than read, so Desk won't use it. Make a key with banks:read and institutions:read only, and set the connection up again.",
            ),
            Self::NotFound(what) => write!(f, "BankSync has no such {what}."),
            Self::RateLimited { retry_after } => write!(
                f,
                "BankSync allows 10 requests a minute. Try again in {retry_after}s."
            ),
            Self::Invalid(why) => f.write_str(why),
            Self::NotConfigured(what) => write!(
                f,
                "This bank connection has no {what}. Set it up again with connections.setup."
            ),
            Self::Unavailable(Some(status)) => {
                write!(f, "BankSync couldn't answer (HTTP {status}). Try again later.")
            }
            Self::Unavailable(None) => f.write_str("BankSync couldn't be reached. Try again later."),
        }
    }
}

impl std::error::Error for BankError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn a_window_defaults_to_the_last_thirty_days_and_refuses_one_backwards() {
        let today = day("2026-11-21");
        assert_eq!(
            Window::of(None, None, today).unwrap(),
            Window {
                from: day("2026-10-22"),
                to: today
            }
        );
        assert_eq!(
            Window::of(None, Some(day("2026-01-31")), today)
                .unwrap()
                .from,
            day("2026-01-01")
        );
        assert!(matches!(
            Window::of(Some(day("2026-02-02")), Some(day("2026-02-01")), today),
            Err(BankError::Invalid(_))
        ));
    }

    #[test]
    fn an_approval_reminds_seven_days_before_the_day_before_and_on_the_day() {
        let consented: DateTime<Utc> = "2026-09-01T10:00:00Z".parse().unwrap();
        let until = day("2027-02-28");
        let reminded: Vec<i64> = (-2..=10)
            .map(|n| until - Duration::days(n))
            .map(|today| Approval::of(consented, today))
            .filter(|a| a.remind)
            .map(|a| a.days_left)
            .collect();
        assert_eq!(Approval::of(consented, until).until, until);
        assert_eq!(reminded, vec![0, 1, 7]);
        let due: Vec<i64> = (-2..=10)
            .map(|n| Approval::of(consented, until - Duration::days(n)))
            .filter(|a| a.due && a.estimated)
            .map(|a| a.days_left)
            .collect();
        assert_eq!(due, (-2..=7).collect::<Vec<_>>());
        let late_utc: DateTime<Utc> = "2026-09-01T23:30:00Z".parse().unwrap();
        assert_eq!(
            Approval::of(late_utc, day("2026-09-01")).days_left,
            CONSENT_DAYS
        );
    }

    #[test]
    fn a_number_is_shown_by_its_last_four() {
        assert_eq!(masked("12-34-56 87654321"), "•••• 4321");
        assert_eq!(masked("GB33BUKB20201555555555"), "•••• 5555");
        assert_eq!(masked("12"), "•••• 12");
    }

    #[test]
    fn a_search_looks_in_what_a_transaction_says() {
        let t = Transaction {
            id: "t".into(),
            date: "2026-11-01".into(),
            description: "CARD PAYMENT".into(),
            merchant: Some("Tesco Stores".into()),
            amount: Number::from(-12),
            currency: "GBP".into(),
            category: None,
            kind: None,
            reference: Some("Rent Nov".into()),
            pending: false,
            card: None,
        };
        assert!(t.mentions("tesco"));
        assert!(t.mentions("rent"));
        assert!(!t.mentions("amazon"));
    }
}
