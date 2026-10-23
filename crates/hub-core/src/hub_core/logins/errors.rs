//! Why a subscription call was not served, as a login reports it.

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use workforce_agent::llm::failover::{SUBSCRIPTION_UNAVAILABLE_MARKER, UNSUPPORTED_REQUEST_MARKER};
use workforce_common::subscription::SubscriptionRoute;

use super::resolve::NoLogin;

/// Why a subscription call was not served. Every variant but
/// [`Self::Unsupported`] is about the payer's login, not the model, and
/// carries [`SUBSCRIPTION_UNAVAILABLE_MARKER`] so the chain moves on without
/// cooling the provider for anyone else.
#[derive(Debug, thiserror::Error)]
pub enum SubscriptionError {
    #[error(
        "{}: the chatgpt provider is off on this hub ([subscription] chatgpt_direct = false)",
        SUBSCRIPTION_UNAVAILABLE_MARKER
    )]
    Disabled,
    #[error("{marker}: {0}", marker = SUBSCRIPTION_UNAVAILABLE_MARKER)]
    NoLogin(NoLogin),
    #[error("{}: {login} is at its usage limit{}", SUBSCRIPTION_UNAVAILABLE_MARKER, until_suffix(.until))]
    LimitReached {
        login: String,
        until: Option<DateTime<Utc>>,
    },
    #[error(
        "{}: {login} must be signed in again: {reason}",
        SUBSCRIPTION_UNAVAILABLE_MARKER
    )]
    LoginInvalid { login: String, reason: String },
    #[error(
        "{}: {login} could not be refreshed: {reason}",
        SUBSCRIPTION_UNAVAILABLE_MARKER
    )]
    RefreshFailed { login: String, reason: String },
    #[error(
        "{}: {model} is not served on a ChatGPT plan{}: {detail}",
        SUBSCRIPTION_UNAVAILABLE_MARKER,
        for_login(.login)
    )]
    ModelNotInPlan {
        model: String,
        login: Option<String>,
        detail: String,
    },
    #[error("{}: {login} was refused: {detail}", SUBSCRIPTION_UNAVAILABLE_MARKER)]
    Forbidden { login: String, detail: String },
    #[error("{marker}: {0}", marker = UNSUPPORTED_REQUEST_MARKER)]
    Uncarried(String),
    #[error(
        "{}: every {} slot on this hub stayed busy for {}s",
        SUBSCRIPTION_UNAVAILABLE_MARKER,
        .route.label(),
        .waited.as_secs()
    )]
    Busy {
        route: SubscriptionRoute,
        waited: Duration,
    },
}

/// How long a throttled login cools when the backend names no wait.
pub const THROTTLE_COOLDOWN: TimeDelta = TimeDelta::minutes(1);

fn until_suffix(until: &Option<DateTime<Utc>>) -> String {
    until
        .map(|t| format!(" until {}", t.to_rfc3339()))
        .unwrap_or_default()
}

fn for_login(login: &Option<String>) -> String {
    login
        .as_ref()
        .map(|login| format!(" for {login}"))
        .unwrap_or_default()
}
