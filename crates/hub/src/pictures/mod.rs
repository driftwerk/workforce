//! Pictures of agents and people. Each has up to two: one chosen for them
//! (in Desk or the web app) and their Slack picture, which the hub fetches
//! and keeps fresh ([`refresh`]). The chosen one is shown when there is one,
//! else the Slack one, else none (clients show a letter). Kept in Postgres;
//! without it there are no pictures.

pub mod refresh;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(all(test, feature = "postgres"))]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use workforce_agent::llm::image::detect_image_media_type;
use workforce_common::settings::WorkforceConfig;
use workforce_gateway_client::PictureInfo;

/// The largest picture taken, chosen or fetched.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

/// Whose picture it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Agent,
    Identity,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Owner {
    pub kind: Kind,
    pub id: String,
}

impl Owner {
    pub fn agent(id: impl Into<String>) -> Self {
        Self {
            kind: Kind::Agent,
            id: id.into(),
        }
    }

    pub fn identity(id: impl Into<String>) -> Self {
        Self {
            kind: Kind::Identity,
            id: id.into(),
        }
    }
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            Kind::Agent => write!(f, "agent {}", self.id),
            Kind::Identity => write!(f, "identity {}", self.id),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Chosen,
    Slack,
}

impl Source {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chosen => "chosen",
            Self::Slack => "slack",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "chosen" => Some(Self::Chosen),
            "slack" => Some(Self::Slack),
            _ => None,
        }
    }
}

/// Why bytes were not taken as a picture.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("A picture can be at most {} MB.", MAX_BYTES / (1024 * 1024))]
    TooLarge,
    #[error("A picture must be a PNG, JPEG or WebP image.")]
    NotAnImage,
}

/// A picture's image: its bytes, what they are, and a tag that changes
/// whenever they do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub etag: String,
}

impl Image {
    /// `bytes` as a picture: a PNG, JPEG or WebP (judged by the bytes, not
    /// by what they were said to be) of at most [`MAX_BYTES`].
    pub fn accept(bytes: Vec<u8>) -> Result<Self, Refusal> {
        if bytes.len() > MAX_BYTES {
            return Err(Refusal::TooLarge);
        }
        let media_type = match detect_image_media_type(&bytes) {
            Some(t @ ("image/png" | "image/jpeg" | "image/webp")) => t,
            _ => return Err(Refusal::NotAnImage),
        };
        Ok(Self {
            media_type: media_type.to_string(),
            etag: etag_of(&bytes),
            bytes,
        })
    }
}

/// A short tag for `bytes`: the same bytes always give the same one.
pub fn etag_of(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..20].to_string()
}

/// The picture shown, with its image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    pub image: Image,
    pub source: Source,
}

/// The picture shown, without its image: what lists carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Face {
    pub etag: String,
    pub source: Source,
    pub at: DateTime<Utc>,
    /// The workspace of the Slack picture held, shown or overridden.
    pub slack: Option<String>,
}

impl Face {
    pub fn info(&self) -> PictureInfo {
        PictureInfo {
            version: self.etag.clone(),
            source: self.source.as_str().to_string(),
            at: self.at.to_rfc3339(),
            slack: self.slack.clone(),
        }
    }
}

/// One stored picture without its image, as the store lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub source: Source,
    pub etag: String,
    pub at: DateTime<Utc>,
    pub origin: Option<String>,
    pub workspace: Option<String>,
}

/// The face an owner holding `kept` is shown with: the chosen picture if
/// there is one, else the Slack one.
pub fn face_of(kept: &[Kept]) -> Option<Face> {
    let slack = kept.iter().find(|k| k.source == Source::Slack);
    let shown = kept.iter().find(|k| k.source == Source::Chosen).or(slack)?;
    Some(Face {
        etag: shown.etag.clone(),
        source: shown.source,
        at: shown.at,
        slack: slack.map(|s| s.workspace.clone().unwrap_or_default()),
    })
}

/// A Slack picture as fetched: what it was fetched for, and from which
/// workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackPicture {
    pub image: Image,
    pub origin: String,
    pub workspace: String,
}

#[async_trait]
pub trait PictureStore: Send + Sync {
    /// The picture `owner` is shown with.
    async fn shown(&self, owner: &Owner) -> Result<Option<Picture>>;
    /// Everything held for owners of `kind`, by owner id.
    async fn kept(&self, kind: Kind) -> Result<HashMap<String, Vec<Kept>>>;
    /// Everything held for `owner`.
    async fn kept_for(&self, owner: &Owner) -> Result<Vec<Kept>>;
    async fn choose(&self, owner: &Owner, image: &Image) -> Result<()>;
    async fn put_slack(&self, owner: &Owner, picture: &SlackPicture) -> Result<()>;
    /// Removes `owner`'s picture from `source`; false when there was none.
    async fn remove(&self, owner: &Owner, source: Source) -> Result<bool>;
    /// Removes everything held for `owner`; how many pictures went.
    async fn forget(&self, owner: &Owner) -> Result<u64>;
    /// Gives `to` the picture chosen for `from`, unless `to` has one chosen
    /// already, and drops the rest of `from`'s; whether `to` took it.
    async fn hand_over(&self, from: &Owner, to: &Owner) -> Result<bool>;
}

/// The face `owner` is shown with now.
pub async fn face(store: &dyn PictureStore, owner: &Owner) -> Result<Option<Face>> {
    Ok(face_of(&store.kept_for(owner).await?))
}

/// The faces of every owner of `kind` that has one.
pub async fn faces(store: &dyn PictureStore, kind: Kind) -> Result<HashMap<String, Face>> {
    Ok(store
        .kept(kind)
        .await?
        .into_iter()
        .filter_map(|(id, kept)| face_of(&kept).map(|face| (id, face)))
        .collect())
}

/// The install's picture store; `None` without Postgres, or when it can't
/// be opened (said in the log).
pub async fn open(config: &WorkforceConfig) -> Option<Arc<dyn PictureStore>> {
    #[cfg(feature = "postgres")]
    if let Some(pg) = config.postgres.as_ref().filter(|p| p.enabled) {
        return match postgres::PostgresPictures::new(pg).await {
            Ok(store) => Some(Arc::new(store) as Arc<dyn PictureStore>),
            Err(e) => {
                tracing::warn!(target: "pictures", error = %format!("{e:#}"), "picture store unavailable; agents and people show no pictures");
                None
            }
        };
    }
    let _ = config;
    None
}
