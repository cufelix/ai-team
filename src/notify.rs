//! "Calling" Felix: desktop notification + optional ntfy.sh push + ad-hoc calendar event.
use crate::config::Config;

pub async fn felix(cfg: &Config, http: &reqwest::Client, title: &str, body: &str, link: &str) {
    if cfg.notify.desktop {
        let _ = tokio::process::Command::new("notify-send")
            .args(["-a", "AI tým", "-u", "critical", title, &format!("{body}\n{link}")])
            .status()
            .await;
    }
    let topic = cfg.notify.ntfy_topic.trim();
    if !topic.is_empty() {
        // Only the short title/body go to ntfy.sh; topic name should be random (it is the only secret).
        let _ = http
            .post(format!("https://ntfy.sh/{topic}"))
            .header("Title", title)
            .header("Click", link)
            .header("Priority", "high")
            .body(body.to_string())
            .send()
            .await;
    }
}
