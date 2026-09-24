//! Fish Audio, both directions: text-to-speech (`POST /v1/tts`) is Jarvis's
//! voice, speech-to-text (`POST /v1/asr`) is his ears for browsers without a
//! speech service of their own (Arc, Safari, Firefox). The key, the voice
//! (`reference_id`) and the model header are server configuration; a client
//! sends text and gets audio, or sends audio and gets text. TTS responses
//! stream (`Transfer-Encoding: chunked`) and are handed back whole for the
//! handler to forward. The API credit balance (`GET /wallet/self/api-credit`)
//! is read for the HUD's credit warning and cached for `CREDIT_TTL`.

use crate::config::VoiceConfig;
use crate::error::{Error, Result};
use reqwest::{Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// How long one read of the credit balance is reused.
pub const CREDIT_TTL: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct FishAudio {
    http: reqwest::Client,
    base_url: String,
    cfg: VoiceConfig,
    credit: Arc<Mutex<Option<(Instant, VoiceCredit)>>>,
}

/// Fish Audio's API credit: what speech and transcription draw on. Separate
/// from the fish.audio platform plan.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct VoiceCredit {
    /// US dollars as Fish reports them, a decimal string (`"12.34"`).
    pub credit_usd: String,
    /// RFC 3339, when this was last read from Fish.
    pub checked_at: String,
}

#[derive(Deserialize)]
struct ApiCredit {
    credit: String,
}

#[derive(Serialize)]
struct TtsBody<'a> {
    text: &'a str,
    reference_id: &'a str,
    format: &'a str,
    latency: &'a str,
}

impl FishAudio {
    pub fn new(cfg: VoiceConfig) -> Result<Self> {
        Self::with_base_url(cfg, "https://api.fish.audio")
    }

    pub fn with_base_url(cfg: VoiceConfig, base_url: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("opus-api/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(FishAudio {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            cfg,
            credit: Arc::default(),
        })
    }

    pub fn voice_id(&self) -> &str {
        &self.cfg.voice_id
    }

    pub fn model(&self) -> Option<&str> {
        self.cfg.model.as_deref()
    }

    /// A 2xx audio response, status already checked.
    pub async fn speak(&self, text: &str, format: &str, latency: &str) -> Result<Response> {
        let mut req = self
            .http
            .post(format!("{}/v1/tts", self.base_url))
            .bearer_auth(&self.cfg.fish_audio_api_key)
            .json(&TtsBody {
                text,
                reference_id: &self.cfg.voice_id,
                format,
                latency,
            });
        if let Some(model) = &self.cfg.model {
            req = req.header("model", model);
        }
        let res = req.send().await?;
        if res.status().is_success() {
            return Ok(res);
        }
        Err(provider_error(res).await)
    }

    /// Speech-to-text. `audio` is a whole recording (WAV, MP3 or M4A — Fish
    /// cannot decode WebM, verified 2026-09-23); `language` a two-letter
    /// hint. Fish's own "could not be decoded" 400 comes back as the
    /// caller's `invalid_request`.
    pub async fn transcribe(
        &self,
        audio: bytes::Bytes,
        mime: &str,
        language: &str,
    ) -> Result<Transcription> {
        let part = reqwest::multipart::Part::bytes(audio.to_vec())
            .file_name("audio")
            .mime_str(mime)?;
        let form = reqwest::multipart::Form::new()
            .part("audio", part)
            .text("language", language.to_owned());
        let res = self
            .http
            .post(format!("{}/v1/asr", self.base_url))
            .bearer_auth(&self.cfg.fish_audio_api_key)
            .multipart(form)
            .send()
            .await?;
        if res.status().is_success() {
            return Ok(res.json::<Transcription>().await?);
        }
        if res.status() == StatusCode::BAD_REQUEST {
            let body: serde_json::Value = res.json().await.unwrap_or_default();
            let message = body["message"]
                .as_str()
                .unwrap_or("the audio could not be decoded")
                .to_owned();
            return Err(Error::InvalidRequest(message));
        }
        Err(provider_error(res).await)
    }
}

impl FishAudio {
    /// The API credit balance, from cache when read within `CREDIT_TTL`.
    /// Failures are not cached, so the next call tries again.
    pub async fn credit(&self) -> Result<VoiceCredit> {
        let mut cached = self.credit.lock().await;
        if let Some((at, credit)) = &*cached {
            if at.elapsed() < CREDIT_TTL {
                return Ok(credit.clone());
            }
        }
        let res = self
            .http
            .get(format!("{}/wallet/self/api-credit", self.base_url))
            .bearer_auth(&self.cfg.fish_audio_api_key)
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(provider_error(res).await);
        }
        let body: ApiCredit = res.json().await?;
        let credit = VoiceCredit {
            credit_usd: body.credit,
            checked_at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        };
        *cached = Some((Instant::now(), credit.clone()));
        Ok(credit)
    }
}

/// What `/v1/asr` answers, minus the parts a caller doesn't need.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct Transcription {
    /// What was said. Empty when nothing intelligible was.
    #[serde(default)]
    pub text: String,
    /// Seconds of audio.
    #[serde(default)]
    pub duration: Option<f64>,
}

/// A non-2xx from Fish, in our error vocabulary (`kind: voice`).
async fn provider_error(res: Response) -> Error {
    let status = res.status();
    let retry_after = res
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok());
    let body = res.text().await.unwrap_or_default();
    let message = match status {
        StatusCode::UNAUTHORIZED => "the voice provider rejected the API key".to_owned(),
        StatusCode::PAYMENT_REQUIRED => "voice credits are exhausted".to_owned(),
        StatusCode::SERVICE_UNAVAILABLE => "the voice provider is overloaded".to_owned(),
        s => format!("voice provider answered {}: {}", s.as_u16(), body.trim()),
    };
    Error::Upstream {
        status: status.as_u16(),
        kind: "voice".into(),
        message,
        retry_after,
    }
}
