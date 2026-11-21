use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: Server,
    pub budget: Budget,
    pub standup: Standup,
    pub notify: Notify,
    pub roles: Vec<Role>,
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Server {
    pub port: u16,
    pub public_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Budget {
    pub max_concurrent: usize,
    pub max_dev_runs_per_day: u32,
    pub plan_every_hours: i64,
    pub tick_minutes: u64,
    pub pause_if_block_cost_usd: f64,
    pub quiet_hours: Vec<u32>,
    pub dev_model: String,
    pub plan_model: String,
    pub meeting_model: String,
    pub dev_timeout_minutes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Standup {
    pub days: Vec<String>,
    pub time: String,
    pub minutes: i64,
    pub timezone_offset: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Notify {
    pub desktop: bool,
    pub ntfy_topic: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub title: String,
    pub voice: String,
    pub persona: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub autonomous: bool,
    pub deadline: String,
    pub deploy_cmd: String,
    pub notes: String,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("{}: {e} — zkopíruj team.example.toml → team.toml", path.display()))?;
        let cfg: Config = toml::from_str(&raw)?;
        anyhow::ensure!(!cfg.roles.is_empty(), "team.toml: no roles");
        for p in &cfg.projects {
            anyhow::ensure!(
                p.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "project id '{}' must be [a-z0-9-]",
                p.id
            );
        }
        Ok(cfg)
    }

    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn role(&self, id: &str) -> Option<&Role> {
        self.roles.iter().find(|r| r.id == id)
    }
}

/// Root of the ai-team checkout (where team.toml lives).
pub fn root() -> PathBuf {
    std::env::var("AI_TEAM_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

pub fn state_dir() -> PathBuf {
    let d = root().join("state");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Loads KEY=VALUE lines from .env into the process env (no override).
pub fn load_dotenv() {
    let Ok(s) = std::fs::read_to_string(root().join(".env")) else { return };
    for line in s.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.is_empty() && !k.starts_with('#') && std::env::var(k).is_err() {
                std::env::set_var(k, v.trim().trim_matches('"'));
            }
        }
    }
}
