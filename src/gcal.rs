//! Google Calendar via OAuth (Desktop client). Until Felix runs `ai-team auth-google`,
//! everything falls back to "add to calendar" template links in notifications.
use anyhow::{bail, Context};
use chrono::{DateTime, FixedOffset, Utc};
use reqwest::Url;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SCOPE: &str = "https://www.googleapis.com/auth/calendar.events";

fn client_path() -> PathBuf { crate::config::state_dir().join("google_client.json") }
fn token_path() -> PathBuf { crate::config::state_dir().join("google_token.json") }

pub fn configured() -> bool { token_path().exists() && client_path().exists() }

fn client() -> anyhow::Result<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(client_path()).with_context(|| {
        format!("chybí {} — stáhni OAuth client (Desktop app) z Google Cloud Console", client_path().display())
    })?)?;
    let c = v.get("installed").or_else(|| v.get("web")).unwrap_or(&v);
    Ok((
        c["client_id"].as_str().context("client_id")?.to_string(),
        c["client_secret"].as_str().context("client_secret")?.to_string(),
    ))
}

/// Interactive one-time login: opens the browser, catches the redirect on localhost.
pub async fn auth(http: &reqwest::Client) -> anyhow::Result<()> {
    let (id, secret) = client()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let url = Url::parse_with_params(
        "https://accounts.google.com/o/oauth2/v2/auth",
        &[("client_id", id.as_str()), ("redirect_uri", &redirect), ("response_type", "code"), ("scope", SCOPE), ("access_type", "offline"), ("prompt", "consent")],
    )?;
    println!("Otevírám prohlížeč pro přihlášení ke Google Kalendáři:\n{url}");
    let _ = std::process::Command::new("xdg-open").arg(url.as_str()).spawn();
    let (mut sock, _) = listener.accept().await?;
    let mut buf = vec![0u8; 8192];
    let n = sock.read(&mut buf).await?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req.split_whitespace().nth(1).unwrap_or("/");
    let code = Url::parse(&format!("http://x{path}"))?
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.to_string())
        .context("redirect bez ?code (zamítnuto?)")?;
    sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<h2>Hotovo, AI t\xc3\xbdm m\xc3\xa1 kalend\xc3\xa1\xc5\x99. M\xc5\xaf\xc5\xbee\xc5\xa1 zav\xc5\x99\xc3\xadt.</h2>").await?;
    let v: serde_json::Value = http
        .post("https://oauth2.googleapis.com/token")
        .form(&[("code", code.as_str()), ("client_id", &id), ("client_secret", &secret), ("redirect_uri", &redirect), ("grant_type", "authorization_code")])
        .send()
        .await?
        .json()
        .await?;
    let refresh = v["refresh_token"].as_str().with_context(|| format!("žádný refresh_token: {v}"))?;
    write_private(&token_path(), &serde_json::json!({ "refresh_token": refresh }).to_string())?;
    println!("Uloženo do {}", token_path().display());
    Ok(())
}

fn write_private(path: &PathBuf, s: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(s.as_bytes())?;
    Ok(())
}

async fn access_token(http: &reqwest::Client) -> anyhow::Result<String> {
    let (id, secret) = client()?;
    let t: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(token_path())?)?;
    let v: serde_json::Value = http
        .post("https://oauth2.googleapis.com/token")
        .form(&[("refresh_token", t["refresh_token"].as_str().unwrap_or("")), ("client_id", &id), ("client_secret", &secret), ("grant_type", "refresh_token")])
        .send()
        .await?
        .json()
        .await?;
    Ok(v["access_token"].as_str().with_context(|| format!("token refresh selhal: {v}"))?.to_string())
}

/// Creates an event in the primary calendar; returns its htmlLink.
pub async fn create_event(http: &reqwest::Client, summary: &str, description: &str, location: &str, start: DateTime<FixedOffset>, minutes: i64) -> anyhow::Result<String> {
    let end = start + chrono::Duration::minutes(minutes);
    let res = http
        .post("https://www.googleapis.com/calendar/v3/calendars/primary/events")
        .bearer_auth(access_token(http).await?)
        .json(&serde_json::json!({
            "summary": summary,
            "description": description,
            "location": location,
            "start": {"dateTime": start.to_rfc3339()},
            "end": {"dateTime": end.to_rfc3339()},
            "reminders": {"useDefault": false, "overrides": [{"method": "popup", "minutes": 5}]}
        }))
        .send()
        .await?;
    if !res.status().is_success() {
        bail!("Calendar {}: {}", res.status(), res.text().await.unwrap_or_default());
    }
    let v: serde_json::Value = res.json().await?;
    Ok(v["htmlLink"].as_str().unwrap_or_default().to_string())
}

/// No-auth fallback: a link that opens Google Calendar with the event pre-filled.
pub fn template_link(summary: &str, description: &str, location: &str, start: DateTime<FixedOffset>, minutes: i64) -> String {
    let f = |d: DateTime<Utc>| d.format("%Y%m%dT%H%M%SZ").to_string();
    let s = start.with_timezone(&Utc);
    let dates = format!("{}/{}", f(s), f(s + chrono::Duration::minutes(minutes)));
    Url::parse_with_params(
        "https://calendar.google.com/calendar/render",
        &[("action", "TEMPLATE"), ("text", summary), ("details", description), ("location", location), ("dates", &dates)],
    )
    .map(|u| u.to_string())
    .unwrap_or_default()
}

/// Event if possible, else template link. Returns the link to show Felix.
pub async fn schedule(http: &reqwest::Client, summary: &str, description: &str, location: &str, start: DateTime<FixedOffset>, minutes: i64) -> (bool, String) {
    if configured() {
        match create_event(http, summary, description, location, start, minutes).await {
            Ok(link) => return (true, link),
            Err(e) => eprintln!("calendar: {e:#}"),
        }
    }
    (false, template_link(summary, description, location, start, minutes))
}
