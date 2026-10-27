//! Stand-ins for the hub's side of the desk routes.

use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};

use crate::api::Authorize;
use crate::realtime::{KeySource, Provider};
use crate::speech::{Audio, Speak, SpeakError, Speech};

/// Lets in `Bearer hub-token` alone.
pub(crate) struct Bearer;

#[async_trait]
impl Authorize for Bearer {
    async fn authorize(&self, h: &HeaderMap, _: Option<&str>) -> Result<(), StatusCode> {
        match h.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer hub-token") => Ok(()),
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }
}

/// Holds no voice keys.
pub(crate) struct NoKeys;

impl KeySource for NoKeys {
    fn api_key(&self, _: Provider) -> Option<String> {
        None
    }
    fn how_to_add(&self, provider: Provider) -> String {
        format!("add speech/{}/api_key", provider.name())
    }
}

/// Speaks nothing.
pub(crate) struct Mute;

#[async_trait]
impl Speech for Mute {
    async fn speak(&self, request: &Speak) -> Result<Audio, SpeakError> {
        Err(SpeakError::NoKey(request.provider))
    }
}
