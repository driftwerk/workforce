# Video generation on Gemini Omni

Status: **design**, 2026-10-27. Replaces the Veo backend of `generate_video`.

## 1. Why

Google has withdrawn every Veo model from the Gemini API. A Driftwerk agent
(Allan) calling `veo-3.1-generate-preview` got:

```
404 This model models/veo-3.1-generate-preview is no longer available.
Please update your code to use models/gemini-omni-1.1-flash
```

On 2026-10-27, `:predictLongRunning` on `veo-3.1-generate-preview`,
`veo-3.1-fast-generate-preview` and `veo-3.1-lite-generate-preview` all
return that same 404 (checked with a deliberately invalid body, so nothing was
billed). `ListModels` lists no `veo-*` model. `GET /models/veo-3.1-…` still
returns `200` with the old metadata, so a model-info probe cannot tell a
retired model from a live one. The video models left are
`gemini-omni-1.1-flash` (GA) and `gemini-omni-flash-preview` (deprecated,
still listed).

Omni is not a drop-in replacement: it is driven through the **Interactions
API** (`POST /v1beta/interactions`), not `:predictLongRunning`, and has a
different request and response shape. Pointing `[llm.video].model` at it
today saves cleanly and then fails on every call. Every `veo:*` tier in
`default_video_tiers.toml` is dead, so `generate_video` is broken on every
install that enables it.

Allan also concluded "the hub needs a restart for the new model to take
effect", which is false. The Driftwerk hub log for 2026-10-27:

| UTC | Event |
|---|---|
| 13:49:32.295 | `Agent config.toml saved` (`via: agent_tool`) — Allan sets `model = "veo:standard"` |
| 13:49:32.327 | `Video generation tool (re)installed`, `model: veo:standard`; `Config hot-reloaded` (both hub replicas within 1s) |
| 13:54:13 | `generate_video` called on the new config |

The reload worked; the call failed because `veo:standard` resolves to
`veo-3.1-fast-generate-preview`, which Google has also retired. Nothing told
Allan that. The save reports `changed: true` and nothing about the media
tools, so the agent explained a failure it couldn't see with a restart. The
save is also silent when `[llm.video]` names a model the registry cannot
resolve: the reload drops `generate_video` with one `warn` in the hub log,
and the save still reports success with no findings.

## 2. Verified against the live API (2026-10-27)

Probed with a paid Gemini key and `Api-Revision: 2026-05-20`, raw REST.
Parameter checks created a background interaction and cancelled it at once,
so only the two generations below were billed.

| Check | Result |
|---|---|
| `background: true` create | `200` in ~2s, `{"id": "v1_…", "status": "in_progress"}` |
| Poll `GET /v1beta/interactions/{id}` | `in_progress` → `completed` |
| Text→video, 360p, default length | completed in **21s**; 10.0s, 640×360, H.264 + AAC, 2.0 MB |
| Image→video, 360p, `duration: "4s"` | completed in **14s**; 4.01s clip, 0.7 MB |
| Output location | `steps[]` → `{"type":"model_output","content":[{"type":"video","mime_type":"video/mp4","data":"<base64>"}]}` |
| Unknown fields | rejected: `400 Unknown parameter 'x' at 'response_format'` |
| `response_format.duration` | string `"Ns"`, **3–10s** (`"2s"` → "less than the minimum allowed 3s"; `"11s"` → "exceeds the maximum allowed 10s"; integer `4` → invalid) |
| `response_format.aspect_ratio` | `"16:9"`, `"9:16"` only (`"1:1"` refused, values listed) |
| `response_format.resolution` | `"360p"`, `"720p"`, `"1080p"`, `"4k"` (`"480p"` refused, values listed) |
| `response_format.delivery: "uri"` | accepted |
| `generation_config.seed` | accepted |
| `person_generation` (anywhere) | **refused** — unknown parameter; there is no such knob |
| `POST /interactions/{id}/cancel` | `200`, status `cancelled` |

Usage reported per call (`usage.output_tokens_by_modality[video]`): 10s at
360p = 19,310 tokens (≈ $0.34 at $17.50/M); 4s at 360p = 7,724 (≈ $0.14).
Google prices 720p at 5,792 tokens/s ≈ **$0.10 per second**, so a default
10s 720p clip is ≈ $1. No free tier.

Documented (not probed) and load-bearing:

- **Recognizable people in uploaded images are not supported**, and images
  containing minors are refused in the EEA, Switzerland and the UK. Taken at
  its word, this ends the shipped "make a video of Tom dancing" flow
  (`get_user_photo` → image→video), which Veo did with
  `personGeneration: allow_adult`. **Not yet probed** — the §8 live check
  settles it before the tool description or `get_user_photo` change.
  Generic people generated from text are unaffected.
- Editing or extending an **uploaded** video is unavailable in the EEA,
  Switzerland and the UK. Editing/extending a video Omni itself generated
  (`previous_interaction_id`) works everywhere.
- Background execution requires `store = true` (the default); stored
  interactions are retained 55 days on paid projects and can be deleted.
- All output carries SynthID.

## 3. Goals and non-goals

Goals:

1. `generate_video` works again, on Omni, with no new config block — the
   existing `[llm.video]` grammar (`provider:tier` or a concrete model) and
   the existing Gemini key path.
2. Existing configs keep working where they can (`veo:standard` etc.), and
   where they can't (a concrete `veo-*` id), the config save says so to
   whoever saved it — agent or human.
3. Text→video and image→video at parity, plus what Omni adds cheaply at the
   request level: several input images (first/last frame, subject
   references), resolution, and 3–10s duration.
4. The Veo backend is deleted, not left beside Omni as dead code.

Non-goals (see §9 for the follow-up design):

- Editing or extending a generated video (`previous_interaction_id`).
- Editing uploaded videos — geo-blocked for Driftwerk (EEA host, UK users).
- Moving generation onto the job runner. Omni returns in 15–60s, so the
  inline call `generate_video` already makes stays acceptable.

## 4. Config

### 4.1 Grammar — unchanged shape, new table

`[llm.video].model` keeps the image/video spec grammar. The tier table names
the backend by the API it is called through (`gemini`), not by a model
family that can be withdrawn again:

```toml
# default_video_tiers.toml
#
# Video generation through the Gemini API's Interactions endpoint. Google
# withdrew every Veo model from the Gemini API in October 2026; Gemini Omni
# is the only video line it serves. All tiers name the one GA model until
# Google ships another.
[gemini]
powerful = "gemini-omni-1.1-flash"
standard = "gemini-omni-1.1-flash"
cheap    = "gemini-omni-1.1-flash"
```

`video_tiers.rs`:

- `pub const GEMINI: &str = "gemini";` replaces `VEO`.
- `is_gemini(provider)` accepts `gemini`, `google`, and `veo`. `veo` stays a
  name for the Google video backend so `veo:standard` / `veo:cheap` keep
  resolving (to Omni); it is documented as a legacy spelling.
- `video_tier_key` maps all three to the `gemini` table; anything else is
  refused as today: `'{provider}' generates no video; the video provider is
  gemini`.

### 4.2 Concrete Veo ids are refused by name

A concrete `veo-…` model id passes the tier registry (bare ids resolve to
themselves) and would 404 at call time. `video::route` refuses it up front:

```
[llm.video] model "veo-3.1-generate-preview": Google withdrew Veo from the
Gemini API (October 2026); use "gemini:standard" (gemini-omni-1.1-flash)
```

Concrete ids that are not `veo-*` (a future `gemini-omni-2-…`) pass through
unchanged, as today.

### 4.3 Fields

`LlmVideoGenerationConfig` (`crates/common/src/settings.rs`):

| Field | Change |
|---|---|
| `enabled`, `provider`, `model`, `api_key`, `aspect_ratio`, `duration_secs` | unchanged |
| `resolution: Option<String>` | **new** — `360p` / `720p` / `1080p` / `4k`; `None` ⇒ Omni's default (720p). Config-only: cost scales with it, so the agent doesn't pick it per call |
| `person_generation` | **removed** — Omni has no such parameter and rejects it. The struct has no `deny_unknown_fields`, so existing configs carrying the key still parse; it is ignored |
| `url`, `region` | unchanged (pre-existing reserved fields, out of scope) |

`aspect_ratio` and `resolution` are validated by `video::route` (§5.1), so a
bad value is a config finding, not a 400 at call time.

## 5. Runtime

### 5.1 `llm/video/mod.rs`

The pure half of provider construction is split out so the install path and
config admission share it (§6):

```rust
/// What an `[llm.video]` block resolves to before any key is involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoRoute {
    pub model: String,
    /// The clips' shape when a call names none.
    pub aspect_ratio: AspectRatio,
    pub resolution: Option<Resolution>,
}

/// Resolve and check `cfg` without building a provider: the model spec, and
/// the values Omni accepts for aspect ratio and resolution.
pub fn route(cfg: &LlmVideoGenerationConfig) -> Result<VideoRoute> { … }

/// Infallible once routed, so the install builds the tool from one `route`.
pub fn provider_for(route: &VideoRoute, key: KeySlot) -> Box<dyn VideoProvider> { … }
```

`route` bails on: disabled block, empty model, unresolvable spec, `veo-*` id
(§4.2), `aspect_ratio` not in `AspectRatio`, `resolution` not in
`Resolution`. `AspectRatio { Landscape, Portrait }` and
`Resolution { P360, P720, P1080, K4 }` are small enums with `FromStr` and
`as_str()` giving the wire values — the one place those strings live.

`VideoRequest` generalises the single starting image:

```rust
pub struct VideoRequest {
    pub prompt: String,
    /// Images in prompt order: one animates it; two are first/last frame;
    /// more are subject references (`<IMAGE_REF_N>` in the prompt).
    pub images: Vec<InputImage>,
    pub aspect_ratio: AspectRatio,
    /// Clamped to Omni's 3–10s; `None` ⇒ Omni's default (10s).
    pub duration_secs: Option<u32>,
}
```

`person_generation` leaves `VideoRequest` and `VideoGenerationTool`. The
module doc drops "1–6 min" and Veo.

### 5.2 `llm/video/omni.rs` (replaces `veo.rs`)

Constants:

```rust
const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
/// Pins the Interactions response schema this client parses.
const API_REVISION: &str = "2026-05-20";
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// 4k is upscaled server-side and slower than the ~20s measured at 360p.
const DEADLINE: Duration = Duration::from_secs(600);
```

`generate_video` declares no `Tool::soft_deadline`, so the runner's
`execute_with_deadline` runs it unbounded; `DEADLINE` is the only time limit,
and a stopped turn drops the future (handled by the cancel guard below).

Auth is the `x-goog-api-key` header on every request, never `?key=`, so the
key stays out of URLs and reqwest errors. Response bodies still pass through
`scrub(…, Some(&key))` before they reach an error.

**Create** — `POST {API_BASE}/interactions`:

```json
{
  "model": "gemini-omni-1.1-flash",
  "background": true,
  "input": [
    {"type": "image", "data": "<b64>", "mime_type": "image/png"},
    {"type": "text", "text": "<prompt>"}
  ],
  "response_format": {
    "type": "video",
    "aspect_ratio": "16:9",
    "resolution": "720p",
    "duration": "6s"
  }
}
```

- `input` is the plain prompt string when there are no images, else image
  parts in order followed by one text part.
- `resolution` / `duration` are omitted when unset (Omni defaults).
- `background: true` because a synchronous call holds one HTTP request open
  for the whole generation, and connections that long get cut (Google's
  background-execution guide cites 60s); polling a stored interaction
  survives that.
- Inline delivery (no `delivery` field): one request fewer than `uri`, and
  `GET /interactions/{id}` returns inline data either way.
- Non-2xx → error with the scrubbed body. Google's 400s name the offending
  parameter and its allowed values (verified above), so they are surfaced
  verbatim.

**Poll** — `GET {API_BASE}/interactions/{id}` every `POLL_INTERVAL` until
`DEADLINE`:

- transport error, `429`, `5xx` → keep polling (the Veo lesson: a live run
  hit a mid-poll 503);
- other non-2xx → error; a 2xx whose body can't be read is fatal, not
  retried (a completed interaction carries the whole clip);
- `status` (the documented set): `completed` → extract; `failed` /
  `cancelled` → error carrying the interaction's `error` object when
  present; `requires_action` → error (nothing here can act), cancelled;
  `in_progress` or any status this client doesn't know → keep polling;
- deadline → cancel (below) and error `Omni generation timed out after 600s`,
  naming the last transient poll failure if there was one.

**Extract** — the first `content` item with `type == "video"` in a
`model_output` step:

- `data` → base64-decode;
- else `uri` → `GET` it, with the key header only when the URI shares
  `API_BASE`'s origin (scheme, host, port); a redirect off that origin is
  followed by a second, keyless client.
- `media_type` from `mime_type`, defaulting to `video/mp4`.

**Refusals — shape unverified.** No refusal has been observed yet. The
client handles all three plausible shapes, each ending in an error the agent
can relay instead of retrying:

- `400` on create → Google's message, scrubbed;
- `failed` → the interaction's `error` object;
- `completed` with no video part → the model's text parts:
  `Omni returned no video: <text>`.

The §8 live check captures the real one as a fixture.

Each poll `GET` returns the whole interaction, including the `user_input`
step, so an image→video poll re-downloads its input images every 5s. That
is a few hundred KB per poll for typical inputs and is accepted; nothing
lighter is documented.

**Redirects** — the key rides a custom header, and reqwest strips only its
standard credential headers (`Authorization`, `Cookie`, …) on a cross-host
redirect. So the keyed client's redirect policy follows only within
`API_BASE`'s origin and hands any other redirect back as the response.

**The stored interaction** — a `Stored { client, url, key, running }` guard
is made after create; `running` clears when the interaction completes,
fails or is cancelled. Its `Drop` spawns, on the current Tokio handle (best
effort, failures at `debug`, off the caller's path):

- `POST /interactions/{id}/cancel` if it was still running — the deadline,
  `requires_action`, or a turn stopped mid-generation (live turn control
  drops the tool future), which would otherwise run on and be billed (~$1);
- then `DELETE /interactions/{id}`, on success and failure alike. Background
  mode forces `store = true`; nothing in this phase reads the interaction
  again, and it holds the request's input images for 55 days otherwise. §9
  revisits this when extension needs the id.

**No `verify_credentials`** — the old trait method had no caller, and a
model lookup can't stand in for one: retired Veo models still answer `200`
(§1). It is removed from `VideoProvider`.

### 5.3 `tools/video_gen.rs`

- `images[]` replaces `image`, same item shape as `generate_image`'s
  `images[]` and resolved by its now-shared `resolve_reference_images`. A
  lone `image` object (the old schema, still in agents' histories and
  know-how) is read as `images: [image]` rather than silently dropped; an
  `images` that isn't an array is a tool failure. Google
  documents no image-count limit (its "max 3" is for video references), so
  the tool sets none; an over-limit request surfaces Google's 400.
- `aspect_ratio` gains `"enum": ["16:9", "9:16"]`; a bad value from the model
  is a tool failure listing the two values, not an API round-trip.
- `duration_secs` description: "3–10; defaults to the configured length".
  Values outside are clamped (stated in the tool result as `duration_secs`
  actually used).
- Description rewritten for Omni: ~15–60s and blocks; one image animates it,
  two set first and last frame, more are references by `<IMAGE_REF_N>`. The
  "you will NOT see the video" delivery note stays.
- If the §8 face probe is refused: the description adds "Omni refuses
  recognizable real people from photos — don't fetch someone's profile photo
  to animate them", and `get_user_photo`
  (`crates/skills/src/adapters/user_photo.rs:65`) drops "make a video of
  Tom" from its description. If it is accepted, both stay as they are.
- Delivery is unchanged: the MP4 is staged and returned under the
  `generated_images` marker for `HubImageSink`.

## 6. Media config findings on save

`review_agent_llm` (`hub_core/subscriptions.rs`) gains media findings:

```rust
let media = media_findings(llm);
```

`media_findings` runs each enabled block through the builder its install
runs, with an empty key (every media provider reads its key per request,
never at build), and turns an error into a `Finding::warning` against the
block — **warning, not refusal**: the agent loads and runs without that tool,
exactly as it does now. Running the real builder, not a pure subset, is what
catches install-only failures such as ElevenLabs without a `voice` or a
vLLM image route without a URL.

| Block | Check (the install's own builder) |
|---|---|
| `[llm.video]` | `video::route` (§5.1; `provider_for` cannot fail) |
| `[llm.image]` | `image::provider_from_config` |
| `[llm.speech]` | `speech::provider_from_config` |

The message names the tool:
`[llm.video] … : generate_video will not be installed: <reason>`.

These findings already flow everywhere a config is written: `system.config`
returns `findings` to the agent, the config API and UI show them, and
environment edits carry them. A save naming a `veo-*` id comes back with the
reason and the replacement model instead of a bare `changed: true`.

## 7. Removals and renames

| Where | Change |
|---|---|
| `llm/video/veo.rs` | deleted (its `download_url` host rule moves to `omni.rs`) |
| `person_generation_candidates`, `is_person_generation_rejection` | deleted with it |
| `video_tiers.rs` `VEO`, `is_veo`, `veo_tier_key` | → `GEMINI`, `is_gemini`, `video_tier_key` |
| `credentials.rs` `VIDEO_PROVIDER` doc | "The provider Omni's key belongs to." Key path, catalog entry (`video/gemini/api_key`) and fallback order unchanged |
| `serve.rs` `video_config` doc | "rebuild the Omni provider" |
| `settings.rs` `LlmVideoGenerationConfig` doc (~5255–5268), `provider` doc (~5274), `LlmConfig.video` doc (~5578) | Omni, not Veo; `provider` = `gemini`/`google` (`veo` legacy); example becomes `model = "gemini:cheap"` (scanned by `secondary_model_specs`); key order unchanged |
| `secondary_model_specs.rs` | imports `GEMINI`; `the_documents_are_read` requires `"gemini:standard"`; the video test resolves documented specs under `GEMINI` |
| `end-user-docs/configuration/tiers.mdx` | Video section: `model = "gemini:standard"`, `resolution`, `duration_secs` 3–10; drop `person_generation`; note `veo:` is a legacy spelling and Veo ids are refused |
| `end-user-docs/reference/agent-types.mdx:275,283` | Omni, image→video without recognizable people; `VideoTierRegistry` line kept |
| `end-user-docs/concepts/agents.mdx:110` | "Google Gemini Omni" |
| `end-user-docs/reference/tools.mdx:54-57` | drop "dancing" as a `generate_video` trigger |
| `docs/VAULT.md:219` | `generate_video (Gemini Omni)` |

## 8. Tests

Unit, in the files they cover (no network):

- `video_tiers.rs`: `gemini:`/`google:`/`veo:` tiers all resolve to
  `gemini-omni-1.1-flash`; bare tier with `Some("gemini")`; non-Google
  provider refused; no-provider refused (existing cases, renamed).
- `video/mod.rs` `route`: tier spec ok; `veo-3.1-generate-preview` refused
  with the replacement named; non-Veo concrete id passes; bad aspect ratio
  and resolution refused listing allowed values; disabled / empty model
  refused (existing).
- `omni.rs` request building: prompt-only → string `input`; images → image
  parts in order then text; `duration` clamped (`1` → `"3s"`, `30` →
  `"10s"`); unset resolution/duration omitted.
- `omni.rs` parsing against a fixture shaped like the probes' responses:
  completed with text and no video → `Omni returned no video: <text>`; only
  a documented end stops polling; the key's origin check.
- `omni.rs` against a mock server (wiremock, as `image/gemini.rs` does):
  a clip rides out a 503 mid-poll, the key is only ever a header, and the
  interaction is deleted, not cancelled; a 400 echoing the key comes back
  scrubbed with Google's reason; a `failed` generation is deleted, not
  cancelled; an abandoned one is cancelled then deleted; a clip redirected
  off the API is fetched without the key.
- `video_gen.rs`: existing install/reload tests on `gemini:standard`;
  several `images[]` reach the request in order; a lone `image` still
  arrives; a non-array `images` and a bad `aspect_ratio` fail before the
  provider is called (stub provider asserts it was not reached).
- `subscriptions.rs`: a `veo-*` video model, a non-generating image
  provider and an ElevenLabs block without a voice each yield one warning
  naming their tool; valid and switched-off blocks yield none; findings
  never refuse.
- `secondary_model_specs.rs`: updated as in §7.

Live (manual, before merge), through the built provider against a real key:

- one 360p text→video and one 360p image→video — **done 2026-10-27**, from
  a `veo:standard` config (Allan's exact value): 3s clips in 15.2s and
  14.0s, 640×360 H.264 + AAC;
- **face probe**: one 3s 360p image→video from a photo of a consenting
  person (the operator's own). It decides §5.3's description change and
  records the refusal shape as a test fixture (~$0.10);
- a turn stopped mid-generation is cancelled (covered against a mock server;
  not yet seen against Google).

Gates: `cargo fmt --check`, `cargo clippy --workspace -D warnings`,
`cargo test` for `workforce-agent`, `workforce-common`, `workforce-hub-core`,
`workforce-hub`.

## 9. Follow-up: edit and extend generated videos

Omni can continue or edit a clip it generated by `previous_interaction_id`
("make the violin invisible", "continue the scene", 3–10s per extension, 40s
total) — and this is not geo-blocked. Shape, for a later phase:

- `generate_video` gains an optional `from: artifact_id` of a previous
  generated clip.
- The staged artifact records the interaction id in its file metadata, and
  the delete-after-download step (§5.2) is dropped for it; retention then
  follows the project's AI Studio setting (55 days default).
- Editing **uploaded** videos stays out while the EEA/UK block applies.

## 10. Rollout

1. Implement on `main` of `workforce-public` per §5–§8; gates green.
2. Live check as in §8.
3. Configs: `veo:*` tier specs keep working with no edit. On Driftwerk
   (hub log, 2026-10-27) the agents with `[llm.video]` are `allan`
   (`veo:standard`), `sarah` and `Minny` (`veo:powerful`) — all tier specs,
   so all resume on Omni at deploy. Moving them to `gemini:*` and dropping
   `person_generation` is tidying, not required.
4. Other installs: per-agent workspace configs are checked at deploy for
   `[llm.video]` blocks the same way.
