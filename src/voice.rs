//! ElevenLabs: speech-to-text (Scribe) and text-to-speech (Flash v2.5, speaks Czech).
use anyhow::{bail, Context};

fn key() -> anyhow::Result<String> {
    std::env::var("ELEVENLABS_API_KEY").context("ELEVENLABS_API_KEY missing (.env)")
}

pub async fn stt(http: &reqwest::Client, audio: Vec<u8>, mime: &str) -> anyhow::Result<String> {
    let part = reqwest::multipart::Part::bytes(audio).file_name("audio.webm").mime_str(mime)?;
    let form = reqwest::multipart::Form::new()
        .text("model_id", "scribe_v1")
        .text("language_code", "ces")
        .text("tag_audio_events", "false")
        .part("file", part);
    let res = http
        .post("https://api.elevenlabs.io/v1/speech-to-text")
        .header("xi-api-key", key()?)
        .multipart(form)
        .send()
        .await?;
    if !res.status().is_success() {
        bail!("STT {}: {}", res.status(), res.text().await.unwrap_or_default());
    }
    let v: serde_json::Value = res.json().await?;
    Ok(v["text"].as_str().unwrap_or_default().trim().to_string())
}

pub async fn tts(http: &reqwest::Client, voice: &str, text: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(voice.chars().all(|c| c.is_ascii_alphanumeric()), "bad voice id");
    let res = http
        .post(format!("https://api.elevenlabs.io/v1/text-to-speech/{voice}?output_format=mp3_44100_64"))
        .header("xi-api-key", key()?)
        .json(&serde_json::json!({
            "text": text,
            "model_id": "eleven_flash_v2_5",
            "language_code": "cs",
            "voice_settings": {"stability": 0.5, "similarity_boost": 0.75}
        }))
        .send()
        .await?;
    if !res.status().is_success() {
        bail!("TTS {}: {}", res.status(), res.text().await.unwrap_or_default());
    }
    Ok(res.bytes().await?.to_vec())
}
