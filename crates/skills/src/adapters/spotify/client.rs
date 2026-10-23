//! Spotify Web API client for playback control.
//!
//! The API controls *whatever the account is playing on*, which is not the same
//! as the speaker in the room. Spotify calls each place it can play a "device",
//! and a device only exists while something is registered with Spotify Connect
//! — the satellite's librespot appears when raspotify is running and vanishes
//! when it is not. So every control call has to answer "which device?" first,
//! and answer it in a way that survives the device having gone away.

use std::sync::Arc;

use eyre::{eyre, Context, Result};
use serde::Deserialize;
use workforce_agent::tools::RateLimitedError;
use workforce_common::settings::SecretStore;
use workforce_oauth_client::OAuthClientCreds;

use super::provider::SpotifyProvider;

/// A place Spotify can play.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Device {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub volume_percent: Option<u8>,
}

#[derive(Deserialize)]
struct Devices {
    #[serde(default)]
    devices: Vec<Device>,
}

/// What is playing, reduced to what is worth saying out loud.
#[derive(Debug, Clone, PartialEq)]
pub struct NowPlaying {
    pub track: String,
    pub artist: String,
    pub is_playing: bool,
    pub device: Option<String>,
}

#[derive(Deserialize)]
struct PlaybackState {
    #[serde(default)]
    is_playing: bool,
    #[serde(default)]
    item: Option<Track>,
    #[serde(default)]
    device: Option<Device>,
}

#[derive(Deserialize)]
struct Track {
    #[serde(default)]
    name: String,
    #[serde(default)]
    uri: String,
    #[serde(default)]
    artists: Vec<Artist>,
}

#[derive(Deserialize)]
struct Artist {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct SearchResults {
    #[serde(default)]
    tracks: Option<Page<Track>>,
    #[serde(default)]
    artists: Option<Page<Named>>,
    #[serde(default)]
    playlists: Option<Page<Named>>,
}

#[derive(Deserialize)]
struct Page<T> {
    #[serde(default = "Vec::new")]
    items: Vec<T>,
}

/// A search result that is only a name and a URI — an artist or a playlist.
#[derive(Deserialize)]
struct Named {
    #[serde(default)]
    name: String,
    #[serde(default)]
    uri: String,
}

/// What to ask Spotify to play.
///
/// The distinction matters more than it looks: a list of track URIs plays those
/// tracks and then stops, while a context plays that thing and keeps going.
/// Asked for "some Michael Jackson" the first answer is one song and silence,
/// which is not what anybody means.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// Specific tracks, played once.
    Tracks(Vec<String>),
    /// An artist, album or playlist — plays on until stopped.
    Context(String),
    /// Resume whatever was paused.
    Resume,
}

/// What kind of thing a request is asking for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// One specific song.
    Track,
    /// Continuous music from an artist.
    Artist,
    /// A genre, era, mood or activity — "eighties hits", "focus music".
    Playlist,
}

impl Kind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "track" | "song" => Some(Self::Track),
            "artist" => Some(Self::Artist),
            "playlist" | "genre" | "mood" => Some(Self::Playlist),
            _ => None,
        }
    }

    /// Spotify's search type for this kind.
    fn search_type(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Artist => "artist",
            Self::Playlist => "playlist",
        }
    }

    /// Whether playing this should shuffle. A named song should not; a body of
    /// work should, or "some Michael Jackson" opens with the same track every
    /// time — which is exactly what it did.
    pub fn shuffles(self) -> bool {
        !matches!(self, Self::Track)
    }
}

/// Whether a request may play somewhere other than the named speaker.
///
/// Asked out loud, "play something" means the speaker being spoken to, and
/// nothing else will do — the person cannot see where the music went and has no
/// way to know why the room is silent. Typed into Slack it means "play my
/// music", and the nearest working device is a reasonable answer.
///
/// Getting this wrong is not theoretical: a restart deregistered the satellite
/// from Spotify Connect, the fallback quietly chose a laptop in another room,
/// and a request made out loud produced music nobody in that room could hear.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fallback {
    /// Play wherever Spotify already is, if the named speaker is missing.
    Allowed,
    /// The named speaker or nothing — a spoken request.
    Refused,
}

impl Fallback {
    /// What the channel a request arrived on implies.
    ///
    /// Only the voice satellite refuses, because only there is the speaker the
    /// point of the request rather than a preference.
    pub fn for_channel(channel: &str) -> Self {
        match channel {
            "voice-pe" => Self::Refused,
            _ => Self::Allowed,
        }
    }
}

/// Choose which device a command should act on.
pub fn choose_device<'a>(
    devices: &'a [Device],
    preferred: &str,
    fallback: Fallback,
    requested: Option<&str>,
) -> Option<&'a Device> {
    // An asked-for device is not a preference, it is the request. Silently
    // playing somewhere else because a default is configured is what made
    // "play some eighties hits on Ash" start playing on Allan, and then explain
    // that it had.
    if let Some(name) = requested {
        return devices.iter().find(|d| matches_device(&d.name, name));
    }
    let named = devices
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(preferred));
    match fallback {
        Fallback::Refused => named,
        Fallback::Allowed => named
            .or_else(|| devices.iter().find(|d| d.is_active))
            .or_else(|| devices.first()),
    }
}

/// Whether a device name matches what was asked for.
///
/// Loose on purpose: people say "my laptop" for "Kingsley's MacBook Pro" and
/// spoken names arrive without apostrophes, so an exact match would reject most
/// of what anyone actually says.
fn matches_device(device: &str, requested: &str) -> bool {
    let device = device.to_ascii_lowercase();
    let requested = requested.trim().to_ascii_lowercase();
    device == requested || device.contains(&requested) || requested.contains(&device)
}

/// Turn a track into the phrase an assistant would say.
pub fn describe(track: &str, artist: &str) -> String {
    match (track.trim(), artist.trim()) {
        ("", "") => "something".to_string(),
        (t, "") => t.to_string(),
        (t, a) => format!("{t} by {a}"),
    }
}

/// Shared state for the Spotify tools: the vault they resolve tokens from and
/// the OAuth app they refresh with.
pub struct SpotifyContext {
    store: Arc<dyn SecretStore>,
    http: reqwest::Client,
    creds: OAuthClientCreds,
    api_base: String,
    preferred_device: String,
}

impl SpotifyContext {
    pub fn new(
        store: Arc<dyn SecretStore>,
        http: reqwest::Client,
        creds: OAuthClientCreds,
        preferred_device: String,
    ) -> Self {
        Self {
            store,
            http,
            creds,
            api_base: SpotifyProvider::new().api_base().to_string(),
            preferred_device,
        }
    }

    pub fn preferred_device(&self) -> &str {
        &self.preferred_device
    }

    /// A bearer token for the connected account, refreshed if it has expired.
    async fn token(&self) -> Result<String> {
        if !super::token_store::is_connected(self.store.as_ref()) {
            return Err(eyre!(
                "Spotify is not connected — run spotify.setup to link the account"
            ));
        }
        super::token_store::build_token_store(
            Arc::clone(&self.store),
            self.creds.clone(),
            self.http.clone(),
        )
        .access_token()
        .await
        .wrap_err("getting a Spotify access token")
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<Option<serde_json::Value>> {
        let token = self.token().await?;
        let request = build_request(
            &self.http,
            &self.api_base,
            method,
            path,
            query,
            body,
            &token,
        )?;
        let response = self
            .http
            .execute(request)
            .await
            .wrap_err("calling Spotify")?;
        let status = response.status();
        let retry_after = crate::adapters::utils::retry_after_secs(response.headers());
        let text = response.text().await.unwrap_or_default();

        if status.is_success() {
            // Playback commands answer 204 with an empty body; only the read
            // endpoints return JSON.
            if text.trim().is_empty() {
                return Ok(None);
            }
            return Ok(serde_json::from_str(&text).ok());
        }
        Err(spotify_error(status, &text, retry_after))
    }

    /// Every device the account can currently play on.
    pub async fn devices(&self) -> Result<Vec<Device>> {
        let body = self
            .request(reqwest::Method::GET, "/me/player/devices", &[], None)
            .await?;
        let Some(body) = body else {
            return Ok(Vec::new());
        };
        Ok(serde_json::from_value::<Devices>(body)
            .map(|d| d.devices)
            .unwrap_or_default())
    }

    /// The device a command should act on, and a clear failure when there is
    /// none — "no device" is the single likeliest thing to go wrong, and it is
    /// fixed by opening Spotify somewhere, which the message has to say.
    async fn target_device(&self, requested: Option<&str>) -> Result<Device> {
        let devices = self.devices().await?;
        let fallback = Fallback::for_channel(&current_channel());
        if let Some(device) = choose_device(&devices, &self.preferred_device, fallback, requested) {
            return Ok(device.clone());
        }
        // A device that was asked for by name and is not there is its own
        // failure: naming the alternatives is more use than a default.
        if let Some(name) = requested {
            let available: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
            return Err(eyre!(
                "Spotify has no device called \"{name}\". Available: {}",
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            ));
        }
        // Two different failures, and telling them apart is the point: one
        // means nothing is available anywhere, the other means the speaker
        // being spoken to is not.
        let speaker = &self.preferred_device;
        Err(match fallback {
            Fallback::Refused => eyre!(
                "The {speaker} speaker is not available to Spotify, so there is nothing to play \
                 on. Open Spotify and select {speaker} once to bring it back."
            ),
            Fallback::Allowed => eyre!(
                "Spotify has nowhere to play — no device is available. Start playing on the \
                 speaker or a phone once, so Spotify knows about it."
            ),
        })
    }

    /// Start or resume playback on the chosen device.
    pub async fn play(
        &self,
        target: Target,
        requested_device: Option<&str>,
        shuffle: bool,
    ) -> Result<Device> {
        let device = self.target_device(requested_device).await?;
        let query: Vec<(&str, String)> = device
            .id
            .clone()
            .map(|id| vec![("device_id", id)])
            .unwrap_or_default();

        // Set before playing: applied afterwards it takes effect from the
        // *next* track, so the first one is always the same song.
        if shuffle {
            let mut shuffle_query = vec![("state", "true".to_string())];
            shuffle_query.extend(query.iter().cloned());
            // Not every device supports it, and failing to shuffle is no reason
            // to refuse to play.
            if let Err(e) = self
                .request(
                    reqwest::Method::PUT,
                    "/me/player/shuffle",
                    &shuffle_query,
                    None,
                )
                .await
            {
                tracing::debug!(error = %e, "could not enable shuffle");
            }
        }

        let body = match &target {
            Target::Tracks(uris) => Some(serde_json::json!({ "uris": uris })),
            Target::Context(uri) => Some(serde_json::json!({ "context_uri": uri })),
            Target::Resume => None,
        };
        self.request(reqwest::Method::PUT, "/me/player/play", &query, body)
            .await?;
        Ok(device)
    }

    pub async fn pause(&self) -> Result<()> {
        self.request(reqwest::Method::PUT, "/me/player/pause", &[], None)
            .await?;
        Ok(())
    }

    /// Skip forward or back. Spotify has no "stop", so pausing is as close as
    /// the API gets and is what people mean anyway.
    pub async fn skip(&self, forward: bool) -> Result<()> {
        let path = if forward {
            "/me/player/next"
        } else {
            "/me/player/previous"
        };
        self.request(reqwest::Method::POST, path, &[], None).await?;
        Ok(())
    }

    pub async fn set_volume(&self, percent: u8) -> Result<()> {
        let percent = percent.min(100);
        self.request(
            reqwest::Method::PUT,
            "/me/player/volume",
            &[("volume_percent", percent.to_string())],
            None,
        )
        .await?;
        Ok(())
    }

    /// What is playing now, or `None` when nothing is.
    pub async fn now_playing(&self) -> Result<Option<NowPlaying>> {
        let Some(body) = self
            .request(reqwest::Method::GET, "/me/player", &[], None)
            .await?
        else {
            return Ok(None);
        };
        let state: PlaybackState =
            serde_json::from_value(body).wrap_err("reading Spotify playback state")?;
        Ok(state.item.map(|track| NowPlaying {
            artist: track
                .artists
                .first()
                .map(|a| a.name.clone())
                .unwrap_or_default(),
            track: track.name,
            is_playing: state.is_playing,
            device: state.device.map(|d| d.name),
        }))
    }

    /// Find something to play, and describe it the way it would be spoken.
    ///
    /// A track is played on its own; an artist or playlist is played as a
    /// context so the music continues instead of stopping after one song.
    pub async fn find(&self, query: &str, kind: Kind) -> Result<(Target, String)> {
        let body = self
            .request(
                reqwest::Method::GET,
                "/search",
                &[
                    ("q", query.to_string()),
                    ("type", kind.search_type().to_string()),
                    ("limit", "1".into()),
                ],
                None,
            )
            .await?
            .ok_or_else(|| eyre!("Spotify returned no search results"))?;
        let results: SearchResults =
            serde_json::from_value(body).wrap_err("reading Spotify search results")?;

        match kind {
            Kind::Track => {
                let track = results
                    .tracks
                    .and_then(|p| p.items.into_iter().next())
                    .ok_or_else(|| eyre!("Spotify has no track matching \"{query}\""))?;
                let artist = track
                    .artists
                    .first()
                    .map(|a| a.name.clone())
                    .unwrap_or_default();
                Ok((
                    Target::Tracks(vec![track.uri]),
                    describe(&track.name, &artist),
                ))
            }
            Kind::Artist => {
                let artist = results
                    .artists
                    .and_then(|p| p.items.into_iter().next())
                    .ok_or_else(|| eyre!("Spotify has no artist matching \"{query}\""))?;
                let name = artist.name.clone();
                Ok((Target::Context(artist.uri), name))
            }
            Kind::Playlist => {
                let playlist = results
                    .playlists
                    .and_then(|p| p.items.into_iter().next())
                    .ok_or_else(|| eyre!("Spotify has no playlist matching \"{query}\""))?;
                let name = playlist.name.clone();
                Ok((Target::Context(playlist.uri), name))
            }
        }
    }
}

/// Build one Spotify request.
///
/// Separate from sending it so the shape of what goes on the wire can be
/// asserted — the `Content-Length` below is invisible from the outside and
/// impossible to notice until Spotify rejects the call.
#[allow(clippy::too_many_arguments)]
fn build_request(
    http: &reqwest::Client,
    api_base: &str,
    method: reqwest::Method,
    path: &str,
    query: &[(&str, String)],
    body: Option<serde_json::Value>,
    token: &str,
) -> Result<reqwest::Request> {
    let needs_length =
        body.is_none() && matches!(method, reqwest::Method::PUT | reqwest::Method::POST);
    let mut request = http
        .request(method, format!("{api_base}{path}"))
        .bearer_auth(token);
    if !query.is_empty() {
        request = request.query(query);
    }
    match body {
        Some(body) => request = request.json(&body),
        // Pause, next and previous take no body, and Spotify rejects them with
        // "411 Length Required" unless a length is declared anyway. The header
        // is set explicitly rather than inferred from an empty body: reqwest
        // fills it in at send time for some bodies and not others, and this has
        // to be certain.
        None if needs_length => {
            request = request
                .header(reqwest::header::CONTENT_LENGTH, "0")
                .body(Vec::new())
        }
        None => {}
    }
    request.build().wrap_err("building the Spotify request")
}

/// The channel this request arrived on, or empty when there is none.
fn current_channel() -> String {
    workforce_agent::runner::CURRENT_CHANNEL_ID
        .try_with(Clone::clone)
        .unwrap_or_default()
}

/// Turn an API failure into something that names the fix.
///
/// Spotify's own error bodies are terse and its status codes carry most of the
/// meaning, and these three are the ones a person can actually act on.
fn spotify_error(
    status: reqwest::StatusCode,
    body: &str,
    retry_after: Option<u64>,
) -> eyre::Report {
    match status.as_u16() {
        401 => eyre!("Spotify rejected the login — reconnect with spotify.setup"),
        // Spotify spells it PREMIUM_REQUIRED in the reason and "Premium" in
        // the message, depending on the endpoint.
        403 if body.to_ascii_lowercase().contains("premium") => {
            eyre!("Spotify playback control needs a Premium account")
        }
        404 => eyre!(
            "Spotify has nowhere to play — the device went away. Start playing on the speaker \
             or a phone once, so Spotify knows about it."
        ),
        429 => {
            RateLimitedError::new("Spotify is rate limiting; try again shortly", retry_after).into()
        }
        _ => eyre!("Spotify returned {status}: {}", body.trim()),
    }
}

/// An HTTP client for Spotify. Its own so a timeout can be set: a control call
/// sits in the path of a spoken turn, where waiting is worse than failing.
pub fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .wrap_err("building the Spotify HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, active: bool) -> Device {
        Device {
            id: Some(format!("id-{name}")),
            name: name.to_string(),
            is_active: active,
            kind: "Speaker".into(),
            volume_percent: Some(50),
        }
    }

    /// Asked out loud to play something, the person means the speaker they are
    /// talking to — not the phone in their pocket that used Spotify last.
    #[test]
    fn the_configured_speaker_wins_over_whatever_is_active() {
        let devices = vec![device("Kingsley's iPhone", true), device("Allan", false)];
        for fallback in [Fallback::Allowed, Fallback::Refused] {
            assert_eq!(
                choose_device(&devices, "Allan", fallback, None)
                    .unwrap()
                    .name,
                "Allan"
            );
        }
    }

    #[test]
    fn the_match_ignores_case() {
        let devices = vec![device("allan", false)];
        assert_eq!(
            choose_device(&devices, "Allan", Fallback::Refused, None)
                .unwrap()
                .name,
            "allan"
        );
    }

    /// The failure that prompted this: a restart deregistered the satellite,
    /// the fallback chose a laptop in another room, and a request made out loud
    /// played music nobody in that room could hear.
    #[test]
    fn a_spoken_request_never_plays_somewhere_else() {
        let devices = vec![device("Ash", true)];
        assert!(
            choose_device(&devices, "Allan", Fallback::Refused, None).is_none(),
            "a spoken request must not be answered on another device"
        );
    }

    /// Typed from Slack, "play my music" is reasonably answered anywhere.
    #[test]
    fn a_typed_request_falls_back_to_the_active_device() {
        let devices = vec![device("Kitchen Echo", false), device("Ash", true)];
        let chosen = choose_device(&devices, "Allan", Fallback::Allowed, None).expect("a device");
        assert_eq!(chosen.name, "Ash", "should pick the active one");
    }

    /// Only the satellite refuses; every other surface is a preference.
    #[test]
    fn only_the_voice_channel_refuses_to_play_elsewhere() {
        assert_eq!(Fallback::for_channel("voice-pe"), Fallback::Refused);
        for typed in ["clawdy", "slack:C123", "web", ""] {
            assert_eq!(Fallback::for_channel(typed), Fallback::Allowed, "{typed}");
        }
    }

    #[test]
    fn no_devices_at_all_is_not_a_choice() {
        assert!(choose_device(&[], "Allan", Fallback::Allowed, None).is_none());
    }

    /// The failure in use: "play me some eighties hits on Ash" started playing
    /// on Allan, then explained that it had. An asked-for device is the
    /// request, not a preference to be overridden by a default.
    #[test]
    fn an_asked_for_device_beats_the_default() {
        let devices = vec![device("Allan", true), device("Ash", false)];
        let chosen = choose_device(&devices, "Allan", Fallback::Allowed, Some("Ash"))
            .expect("the device that was asked for");
        assert_eq!(chosen.name, "Ash");
    }

    /// Named devices are said aloud and typed casually, so matching has to
    /// survive both.
    #[test]
    fn a_device_is_matched_the_way_people_name_it() {
        let devices = vec![device("Kingsley's MacBook Pro", false)];
        for asked in ["macbook", "Kingsley's MacBook Pro", "MACBOOK PRO"] {
            assert!(
                choose_device(&devices, "Allan", Fallback::Allowed, Some(asked)).is_some(),
                "{asked}"
            );
        }
    }

    /// A device that was asked for and is not there must not quietly become a
    /// different one — that is the same silent substitution, one step later.
    #[test]
    fn an_unknown_named_device_is_not_substituted() {
        let devices = vec![device("Allan", true)];
        assert!(choose_device(&devices, "Allan", Fallback::Allowed, Some("Kitchen")).is_none());
    }

    /// Asked for an artist or a genre, playing one track leaves silence after
    /// three minutes. Only a named song should stop.
    #[test]
    fn only_a_named_song_stops_after_one_track() {
        assert!(!Kind::Track.shuffles());
        assert!(Kind::Artist.shuffles());
        assert!(Kind::Playlist.shuffles());
    }

    #[test]
    fn the_kinds_are_parsed_from_what_the_agent_writes() {
        assert_eq!(Kind::parse("artist"), Some(Kind::Artist));
        assert_eq!(Kind::parse("Song"), Some(Kind::Track));
        assert_eq!(Kind::parse("genre"), Some(Kind::Playlist));
        assert_eq!(Kind::parse("nonsense"), None);
    }

    #[test]
    fn a_track_is_described_the_way_it_would_be_spoken() {
        assert_eq!(
            describe("Teardrop", "Massive Attack"),
            "Teardrop by Massive Attack"
        );
        assert_eq!(describe("Teardrop", ""), "Teardrop");
        assert_eq!(describe("", ""), "something");
    }

    fn built(method: reqwest::Method, body: Option<serde_json::Value>) -> reqwest::Request {
        build_request(
            &reqwest::Client::new(),
            "https://api.spotify.com/v1",
            method,
            "/me/player/pause",
            &[],
            body,
            "t",
        )
        .expect("a request")
    }

    /// The bug this exists to prevent: Spotify answers "411 Length Required"
    /// to a PUT or POST with no body and no `Content-Length`, and reqwest omits
    /// the header entirely unless a body is set. Pause, next and previous all
    /// take no body, so every one of them failed.
    #[test]
    fn bodyless_commands_still_declare_a_content_length() {
        for method in [reqwest::Method::PUT, reqwest::Method::POST] {
            let request = built(method.clone(), None);
            assert_eq!(
                request.headers().get(reqwest::header::CONTENT_LENGTH),
                Some(&reqwest::header::HeaderValue::from_static("0")),
                "{method} with no body must send Content-Length: 0"
            );
        }
    }

    /// A GET has nothing to declare, and sending a length on one is at best
    /// noise.
    #[test]
    fn reads_do_not_carry_a_content_length() {
        let request = built(reqwest::Method::GET, None);
        assert!(request
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .is_none());
    }

    #[test]
    fn a_request_with_a_body_is_left_alone() {
        let request = built(
            reqwest::Method::PUT,
            Some(serde_json::json!({ "uris": ["x"] })),
        );
        assert_eq!(
            request.headers().get(reqwest::header::CONTENT_TYPE),
            Some(&reqwest::header::HeaderValue::from_static(
                "application/json"
            ))
        );
        assert!(request.body().is_some());
    }

    /// The token has to reach the wire, and a typo here fails as a 401 that
    /// reads like an expired login.
    #[test]
    fn every_request_carries_the_bearer_token() {
        let request = built(reqwest::Method::GET, None);
        let auth = request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .unwrap();
        assert_eq!(auth.to_str().unwrap(), "Bearer t");
    }

    /// These are the failures a person can act on, so they have to say what to
    /// do rather than repeat a status code.
    #[test]
    fn api_failures_name_the_fix() {
        let no_device = spotify_error(reqwest::StatusCode::NOT_FOUND, "", None).to_string();
        assert!(no_device.contains("nowhere to play"), "{no_device}");

        let expired = spotify_error(reqwest::StatusCode::UNAUTHORIZED, "", None).to_string();
        assert!(expired.contains("spotify.setup"), "{expired}");

        let free = spotify_error(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error":{"reason":"PREMIUM_REQUIRED"}}"#,
            None,
        )
        .to_string();
        assert!(free.contains("Premium"), "{free}");
    }

    #[test]
    fn a_429_maps_to_a_typed_rate_limit_with_the_retry_after_hint() {
        let err = spotify_error(reqwest::StatusCode::TOO_MANY_REQUESTS, "", Some(30));
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 should carry the typed rate-limit signal");
        assert_eq!(limit.retry_after_secs, Some(30));
        assert_eq!(limit.message, "Spotify is rate limiting; try again shortly");
    }
}
