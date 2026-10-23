//! Builtin speech synthesis — Piper running locally as a subprocess.
//!
//! Server-side counterpart of the mobile app's "built-in, download the model
//! file" TTS option. On first use the provider downloads (into `data_dir`,
//! default `{workspace}/piper`):
//!
//!   - the self-contained Piper release tarball for this platform (binary +
//!     bundled libespeak-ng + espeak-ng-data), extracted with the system
//!     `tar` (the same shell-out pattern as the ffmpeg/ffprobe probes), and
//!   - the requested voice (`{voice}.onnx` + `{voice}.onnx.json`) from the
//!     `rhasspy/piper-voices` Hugging Face repo.
//!
//! Roughly 20 MB for the runtime and 20–120 MB per voice, one-time. Synthesis
//! is `echo text | piper --model voice.onnx --output_file out.wav` in a temp
//! dir; output is WAV (Piper's native container).

use std::path::{Path, PathBuf};

use eyre::{bail, eyre, Result};
use tokio::io::AsyncWriteExt;
use workforce_common::settings::LlmSpeechConfig;

use super::{SpeechOutput, SpeechProvider, SpeechRequest};

const PIPER_RELEASE_BASE: &str = "https://github.com/rhasspy/piper/releases/download/2023.11.14-2";
const VOICES_BASE: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main";
const DEFAULT_VOICE: &str = "en_US-lessac-medium";
/// Bound on a single synthesis run — CPU inference of a few sentences takes
/// seconds; a hang here should fail the tool call, not wedge the turn.
const SYNTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

pub struct PiperSpeech {
    data_dir: PathBuf,
    voice: String,
    speed: Option<f32>,
    client: reqwest::Client,
    /// Serialises first-use downloads so concurrent synthesis calls don't
    /// race on the same binary/voice files.
    install_lock: tokio::sync::Mutex<()>,
}

/// The Piper release asset for this platform, or an error on platforms the
/// project doesn't ship binaries for.
fn platform_asset() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "piper_linux_x86_64.tar.gz",
        ("linux", "aarch64") => "piper_linux_aarch64.tar.gz",
        ("macos", "aarch64") => "piper_macos_aarch64.tar.gz",
        ("macos", "x86_64") => "piper_macos_x64.tar.gz",
        (os, arch) => bail!("builtin speech has no Piper binary for {os}/{arch}"),
    })
}

/// `pt_BR-faber-medium` → (`pt/pt_BR/faber/medium`, validated). Piper voice
/// ids are `{locale}-{name}-{quality}` and the Hugging Face repo nests them
/// under `{lang}/{locale}/{name}/{quality}/`.
///
/// The id is interpolated into both a download URL and filesystem paths under
/// `data_dir`, and `[llm.speech]` is settable from per-agent config — so each
/// segment is restricted to identifier characters, rejecting `/`, `.`, and
/// anything else that could traverse out of either namespace.
fn voice_repo_path(voice: &str) -> Result<String> {
    let parts: Vec<&str> = voice.split('-').collect();
    let [locale, name, quality] = parts.as_slice() else {
        bail!(
            "invalid Piper voice id '{voice}' — expected {{locale}}-{{name}}-{{quality}}, \
             e.g. 'pt_BR-faber-medium'"
        );
    };
    let identifier =
        |s: &&str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ![locale, name, quality].into_iter().all(identifier) {
        bail!(
            "invalid Piper voice id '{voice}' — each segment may only contain letters, \
             digits, and underscores"
        );
    }
    let lang = locale.split('_').next().unwrap_or(locale);
    Ok(format!("{lang}/{locale}/{name}/{quality}"))
}

impl PiperSpeech {
    pub fn from_config(cfg: &LlmSpeechConfig, speech_home: &Path) -> Self {
        Self {
            data_dir: cfg
                .data_dir
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| speech_home.to_path_buf()),
            voice: cfg
                .voice
                .clone()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_VOICE.to_string()),
            speed: cfg.speed,
            // Generous timeout: first use pulls the ~20 MB runtime and a
            // 20–120 MB voice model.
            client: crate::llm::image::http_client(600),
            install_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn binary_path(&self) -> PathBuf {
        self.data_dir.join("piper").join("piper")
    }

    fn voice_onnx_path(&self, voice: &str) -> PathBuf {
        self.data_dir.join("voices").join(format!("{voice}.onnx"))
    }

    async fn download_to(&self, url: &str, dest: &Path) -> Result<()> {
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| eyre!("download of {url} failed: {e}"))?;
        if !response.status().is_success() {
            bail!("download of {url} returned {}", response.status());
        }
        let parent = dest.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await?;
        // Stream to a uniquely-named temp file in the destination directory
        // (same filesystem → atomic persist). Uniqueness matters: two provider
        // instances can share a data_dir (a hot-reload rebuilds the tool while
        // the old instance is mid-download), and a fixed temp name would let
        // them interleave writes and rename a corrupt file into place.
        let tmp = tempfile::Builder::new()
            .prefix(".download-")
            .tempfile_in(parent)?;
        let mut file = tokio::fs::File::from_std(tmp.reopen()?);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| eyre!("download of {url} body read failed: {e}"))?
        {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);
        tmp.persist(dest)
            .map_err(|e| eyre!("failed to move download into place: {e}"))?;
        Ok(())
    }

    /// Download + extract the Piper runtime and the configured voice if
    /// either is missing. Called before every synthesis; a no-op once both
    /// exist. `voice` is the effective voice for this request (a per-user
    /// override or the configured default) — each distinct voice gets its own
    /// one-time download.
    async fn ensure_installed(&self, voice: &str) -> Result<()> {
        let _guard = self.install_lock.lock().await;

        // Validate the voice id up front, even when the files already exist:
        // every filesystem path below derives from it — including a per-user
        // override, which is user-controlled input.
        let repo_path = voice_repo_path(voice)?;

        let binary = self.binary_path();
        if !binary.exists() {
            let asset = platform_asset()?;
            tracing::info!(asset, dir = %self.data_dir.display(), "downloading Piper runtime (first use)");
            let tarball = self.data_dir.join(asset);
            self.download_to(&format!("{PIPER_RELEASE_BASE}/{asset}"), &tarball)
                .await?;
            let output = tokio::process::Command::new("tar")
                .arg("xzf")
                .arg(&tarball)
                .arg("-C")
                .arg(&self.data_dir)
                .output()
                .await
                .map_err(|e| eyre!("tar not available to extract Piper runtime: {e}"))?;
            let _ = tokio::fs::remove_file(&tarball).await;
            if !output.status.success() {
                bail!(
                    "extracting Piper runtime failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            if !binary.exists() {
                bail!(
                    "Piper tarball extracted but binary not found at {}",
                    binary.display()
                );
            }
        }

        let onnx = self.voice_onnx_path(voice);
        let onnx_json = onnx.with_extension("onnx.json");
        if !onnx.exists() || !onnx_json.exists() {
            tracing::info!(voice = %voice, "downloading Piper voice (first use)");
            // Config first, model last: the model is the larger download, and
            // gating on both files means a failure between the two re-runs
            // this block instead of leaving a half-installed voice.
            self.download_to(
                &format!("{VOICES_BASE}/{repo_path}/{voice}.onnx.json"),
                &onnx_json,
            )
            .await?;
            self.download_to(&format!("{VOICES_BASE}/{repo_path}/{voice}.onnx"), &onnx)
                .await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl SpeechProvider for PiperSpeech {
    async fn synthesize(&self, req: &SpeechRequest<'_>) -> Result<SpeechOutput> {
        let voice = req.voice.as_deref().unwrap_or(&self.voice);
        if req.voice.is_some() && voice != self.voice {
            // A per-USER voice must already be on disk. Only the operator's
            // configured voice may trigger a download: per-user overrides are
            // arbitrary user input, and letting them pull 20–120 MB models on
            // demand is an unbounded disk-fill vector (plus an every-request
            // re-download loop when the id doesn't exist upstream).
            voice_repo_path(voice)?;
            let onnx = self.voice_onnx_path(voice);
            if !onnx.exists() || !onnx.with_extension("onnx.json").exists() {
                bail!(
                    "voice '{voice}' is not installed on this agent — the built-in speech \
                     engine only downloads the operator-configured voice; ask the operator \
                     to set it in [llm.speech] (or pick the default by removing your \
                     personal voice in Settings → Integrations)"
                );
            }
        } else {
            self.ensure_installed(voice).await?;
        }

        // Piper treats each stdin line as a separate utterance; collapse all
        // whitespace so the whole text is one utterance in one output file.
        let text = req.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            bail!("nothing to speak after normalization");
        }

        let out_dir = tempfile::tempdir()?;
        let out_path = out_dir.path().join("speech.wav");
        let mut cmd = tokio::process::Command::new(self.binary_path());
        cmd.arg("--model")
            .arg(self.voice_onnx_path(voice))
            .arg("--output_file")
            .arg(&out_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            // A timed-out run drops the child future below — without this the
            // hung piper process would outlive the tool call and leak.
            .kill_on_drop(true);
        if let Some(speed) = req.speed.or(self.speed).filter(|s| *s > 0.0) {
            let speed = speed.clamp(0.25, 4.0);
            // Piper scales duration, not rate: length_scale 0.5 = twice as fast.
            cmd.arg("--length_scale").arg(format!("{}", 1.0 / speed));
        }

        let mut child = cmd.spawn().map_err(|e| {
            eyre!(
                "failed to run Piper at {}: {e}",
                self.binary_path().display()
            )
        })?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| eyre!("piper stdin unavailable"))?;
        stdin.write_all(text.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        drop(stdin);

        let output = tokio::time::timeout(SYNTH_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| eyre!("piper timed out after {}s", SYNTH_TIMEOUT.as_secs()))??;
        if !output.status.success() {
            bail!(
                "piper exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let bytes = tokio::fs::read(&out_path)
            .await
            .map_err(|e| eyre!("piper produced no output wav: {e}"))?;
        if bytes.is_empty() {
            bail!("piper produced an empty wav");
        }
        Ok(SpeechOutput {
            bytes,
            media_type: "audio/wav",
        })
    }

    fn voice_label(&self) -> String {
        format!("piper/{}", self.voice)
    }

    fn provider_id(&self) -> &'static str {
        "builtin"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_repo_path_derivation() {
        assert_eq!(
            voice_repo_path("pt_BR-faber-medium").unwrap(),
            "pt/pt_BR/faber/medium"
        );
        assert_eq!(
            voice_repo_path("en_US-lessac-medium").unwrap(),
            "en/en_US/lessac/medium"
        );
        assert!(voice_repo_path("not-a-voice-id-extra-parts").is_err());
        assert!(voice_repo_path("ptBRfaber").is_err());
    }

    #[test]
    fn voice_repo_path_rejects_traversal_and_separators() {
        // Three dash-separated segments, but hostile content — must not pass.
        assert!(voice_repo_path("../../../../some/path/x-a-b").is_err());
        assert!(voice_repo_path("..-..-..").is_err());
        assert!(voice_repo_path("pt_BR/../..-faber-medium").is_err());
        assert!(voice_repo_path("pt.BR-faber-medium").is_err());
        assert!(voice_repo_path("--").is_err());
    }

    #[test]
    fn paths_and_defaults() {
        let cfg = LlmSpeechConfig {
            provider: "builtin".into(),
            ..Default::default()
        };
        let p = PiperSpeech::from_config(&cfg, Path::new("/data/agent/piper"));
        assert_eq!(p.voice, DEFAULT_VOICE);
        assert_eq!(p.binary_path(), Path::new("/data/agent/piper/piper/piper"));
        assert_eq!(
            p.voice_onnx_path(&p.voice),
            Path::new("/data/agent/piper/voices/en_US-lessac-medium.onnx")
        );
    }

    #[test]
    fn explicit_data_dir_wins_over_speech_home() {
        let cfg = LlmSpeechConfig {
            provider: "builtin".into(),
            voice: Some("pt_BR-faber-medium".into()),
            data_dir: Some("/opt/tts".into()),
            ..Default::default()
        };
        let p = PiperSpeech::from_config(&cfg, Path::new("/data/agent/piper"));
        assert_eq!(p.binary_path(), Path::new("/opt/tts/piper/piper"));
        assert_eq!(p.voice_label(), "piper/pt_BR-faber-medium");
    }

    #[test]
    fn platform_asset_known_on_dev_and_prod_targets() {
        // The test itself runs on one of the supported platforms in CI/dev.
        assert!(platform_asset().is_ok());
    }

    #[tokio::test]
    async fn per_user_voice_override_never_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LlmSpeechConfig {
            provider: "builtin".into(),
            voice: Some("pt_BR-faber-medium".into()),
            ..Default::default()
        };
        let p = PiperSpeech::from_config(&cfg, dir.path());
        // A valid-format but uninstalled per-user voice must error immediately
        // (no download attempt — the data dir stays empty).
        let err = p
            .synthesize(&SpeechRequest {
                text: "olá",
                voice: Some("de_DE-thorsten-high".into()),
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not installed"), "{err}");
        assert!(
            !dir.path().join("voices").exists(),
            "override must not download"
        );
        // Hostile override goes through the same validation.
        let err = p
            .synthesize(&SpeechRequest {
                text: "olá",
                voice: Some("../../evil/x-a-b".into()),
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid Piper voice id"), "{err}");
    }
}
