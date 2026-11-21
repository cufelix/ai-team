//! Headless Claude Code runner (`claude -p`). Uses Felix's subscription login, no API key.
use anyhow::{bail, Context};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

pub struct Run<'a> {
    pub cwd: &'a Path,
    pub prompt: String,
    pub model: &'a str,
    pub system: Option<String>,
    pub resume: Option<String>,
    /// None = read-only tools (planning / meeting). Some = full dev toolset.
    pub dev_mode: bool,
    pub timeout: Duration,
}

pub struct Output {
    pub text: String,
    pub session_id: String,
}

#[derive(Debug)]
pub struct RateLimited;
impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "claude usage limit reached")
    }
}
impl std::error::Error for RateLimited {}

const READ_ONLY: &[&str] = &["Read", "Glob", "Grep", "Bash(git log:*)", "Bash(git status:*)", "Bash(git diff:*)", "Bash(ls:*)", "Bash(gh pr list:*)", "Bash(gh pr view:*)"];
// Dev agents get edits + shell + subagents, but publishing to the outside world stays with the orchestrator / Felix.
const DEV_ALLOWED: &[&str] = &["Read", "Glob", "Grep", "Edit", "Write", "MultiEdit", "NotebookEdit", "Bash", "Agent", "Task", "TodoWrite", "WebSearch", "WebFetch"];
const DEV_DENIED: &[&str] = &[
    "Bash(git push:*)", "Bash(git push)", "Bash(gh pr merge:*)", "Bash(gh pr create:*)", "Bash(gh release:*)",
    "Bash(git checkout main:*)", "Bash(git checkout master:*)", "Bash(git reset --hard:*)",
    "Bash(firebase deploy:*)", "Bash(vercel:*)", "Bash(wrangler deploy:*)", "Bash(npm publish:*)", "Bash(cargo publish:*)",
    "Bash(rm -rf /:*)", "Bash(sudo:*)",
];

pub async fn run(r: Run<'_>) -> anyhow::Result<Output> {
    let mut cmd = Command::new("claude");
    cmd.current_dir(r.cwd)
        .arg("-p")
        .arg(&r.prompt)
        .args(["--output-format", "json", "--model", r.model])
        .env("AI_TEAM_AGENT", "1")
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());
    if let Some(s) = &r.system {
        cmd.args(["--append-system-prompt", s]);
    }
    if let Some(id) = &r.resume {
        cmd.args(["--resume", id]);
    }
    if r.dev_mode {
        cmd.args(["--permission-mode", "acceptEdits", "--allowedTools"]).args(DEV_ALLOWED);
        // The deny-list is advisory (shell can route around prefixes). Hard stop: the agent process has
        // no working GitHub/git push credentials. ponytail: deploy CLIs with their own stored logins
        // (firebase, vercel…) are still reachable — run agents as a separate user/container if that matters.
        cmd.env("GH_TOKEN", "disabled-for-agents")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "credential.helper")
            .env("GIT_CONFIG_VALUE_0", "")
            .env("GIT_SSH_COMMAND", "false")
            .env_remove("SSH_AUTH_SOCK");
        cmd.arg("--disallowedTools").args(DEV_DENIED);
    } else {
        cmd.arg("--allowedTools").args(READ_ONLY);
    }
    let out = tokio::time::timeout(r.timeout, cmd.output())
        .await
        .context("claude timed out")?
        .context("failed to start `claude`")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lower = format!("{stdout}{stderr}").to_lowercase();
    if lower.contains("usage limit") || lower.contains("rate limit") || lower.contains("limit reached") {
        return Err(RateLimited.into());
    }
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .with_context(|| format!("claude returned non-JSON (exit {}): {}", out.status, truncate(&format!("{stdout}{stderr}"), 400)))?;
    if v["is_error"].as_bool().unwrap_or(false) {
        bail!("claude error: {}", truncate(v["result"].as_str().unwrap_or(""), 400));
    }
    Ok(Output {
        text: v["result"].as_str().unwrap_or_default().to_string(),
        session_id: v["session_id"].as_str().unwrap_or_default().to_string(),
    })
}

/// Extracts the last top-level JSON object from free text (models wrap JSON in prose/fences).
pub fn last_json(text: &str) -> Option<serde_json::Value> {
    let bytes = text.as_bytes();
    let mut end = text.len();
    while let Some(close) = text[..end].rfind('}') {
        // walk back to the matching '{', respecting strings
        let mut depth = 0i32;
        let mut in_str = false;
        let mut i = close as isize;
        while i >= 0 {
            let c = bytes[i as usize];
            if c == b'"' && (i == 0 || bytes[i as usize - 1] != b'\\') {
                in_str = !in_str;
            } else if !in_str {
                if c == b'}' { depth += 1 }
                if c == b'{' {
                    depth -= 1;
                    if depth == 0 {
                        if let Ok(v) = serde_json::from_str(&text[i as usize..=close]) {
                            return Some(v);
                        }
                        break;
                    }
                }
            }
            i -= 1;
        }
        end = close;
    }
    None
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect::<String>() + "…" }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_last_json() {
        let t = "Hotovo.\n```json\n{\"a\":1}\n```\nA finálně: {\"status\":\"done\",\"summary\":\"x {y}\"} díky";
        assert_eq!(last_json(t).unwrap()["status"], "done");
        assert_eq!(last_json("{\"replies\":[{\"t\":\"a}\"}]}").unwrap()["replies"][0]["t"], "a}");
        assert!(last_json("nic").is_none());
    }
}
