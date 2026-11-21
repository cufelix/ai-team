//! Shared team state: tasks, per-project status, budget counters, activity log.
//! Persisted as one JSON file. ponytail: whole-file rewrite on each save, fine for hundreds of tasks.
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Todo,
    InProgress,
    Review,     // PR open, waiting for Felix
    NeedsHuman, // blocked on Felix
    Done,
    Dropped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: u64,
    pub project: String,
    pub title: String,
    pub description: String,
    pub priority: u8, // 1 = highest
    pub role: String,
    pub status: Status,
    #[serde(default)]
    pub due: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub pr_url: String,
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub human_ask: String,
    #[serde(default)]
    pub attempts: u32,
    pub updated: DateTime<Local>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectState {
    pub last_plan: Option<DateTime<Local>>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub deadline: String,
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub at: DateTime<Local>,
    pub who: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Board {
    pub next_id: u64,
    pub tasks: Vec<Task>,
    pub projects: BTreeMap<String, ProjectState>,
    pub runs_day: Option<NaiveDate>,
    pub runs_today: u32,
    pub paused_until: Option<DateTime<Local>>,
    pub meeting_session: Option<(NaiveDate, String)>,
    #[serde(default)]
    pub calendar_events: Vec<String>, // keys of standups already created
    pub log: Vec<Event>,
}

impl Board {
    pub fn log(&mut self, who: &str, text: impl Into<String>) {
        let text = text.into();
        eprintln!("[{}] {who}: {text}", Local::now().format("%H:%M"));
        self.log.push(Event { at: Local::now(), who: who.into(), text });
        let n = self.log.len();
        if n > 300 {
            self.log.drain(..n - 300);
        }
    }

    /// Adds a task unless an open one with the same title already exists in the project.
    pub fn add_task(&mut self, project: &str, title: &str, description: &str, priority: u8, role: &str, due: &str) -> Option<u64> {
        let norm = |s: &str| s.trim().to_lowercase();
        if self.tasks.iter().any(|t| {
            t.project == project && norm(&t.title) == norm(title) && !matches!(t.status, Status::Done | Status::Dropped)
        }) {
            return None;
        }
        self.next_id += 1;
        self.tasks.push(Task {
            id: self.next_id,
            project: project.into(),
            title: title.trim().into(),
            description: description.into(),
            priority: priority.clamp(1, 5),
            role: role.into(),
            status: Status::Todo,
            due: due.into(),
            branch: String::new(),
            pr_url: String::new(),
            result: String::new(),
            human_ask: String::new(),
            attempts: 0,
            updated: Local::now(),
        });
        Some(self.next_id)
    }

    pub fn task_mut(&mut self, id: u64) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    pub fn open_tasks(&self, project: &str) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|t| t.project == project && !matches!(t.status, Status::Done | Status::Dropped))
            .collect()
    }

    /// Next task to work on: autonomous projects only, by due date then priority.
    pub fn pick_next(&self, allowed_projects: &[String]) -> Option<u64> {
        let busy: Vec<&str> = self
            .tasks
            .iter()
            .filter(|t| t.status == Status::InProgress)
            .map(|t| t.project.as_str())
            .collect();
        self.tasks
            .iter()
            .filter(|t| t.status == Status::Todo && t.attempts < 2)
            .filter(|t| allowed_projects.contains(&t.project) && !busy.contains(&t.project.as_str()))
            .filter(|t| !self.projects.get(&t.project).map(|p| p.paused).unwrap_or(false))
            .min_by_key(|t| (if t.due.is_empty() { "9999".to_string() } else { t.due.clone() }, t.priority, t.id))
            .map(|t| t.id)
    }

    pub fn bump_run_counter(&mut self) -> u32 {
        let today = Local::now().date_naive();
        if self.runs_day != Some(today) {
            self.runs_day = Some(today);
            self.runs_today = 0;
        }
        self.runs_today += 1;
        self.runs_today
    }

    pub fn runs_today(&self) -> u32 {
        if self.runs_day == Some(Local::now().date_naive()) { self.runs_today } else { 0 }
    }

    /// Compact text snapshot for LLM prompts.
    pub fn snapshot(&self, project: Option<&str>) -> String {
        let mut out = String::new();
        for (pid, ps) in &self.projects {
            if project.is_some_and(|p| p != pid) {
                continue;
            }
            out += &format!("## {pid} (deadline: {}){}\n{}\n", or_dash(&ps.deadline), if ps.paused { " [PAUSED]" } else { "" }, ps.summary);
            for t in self.tasks.iter().filter(|t| &t.project == pid && t.status != Status::Dropped) {
                if t.status == Status::Done && (Local::now() - t.updated).num_days() > 3 {
                    continue;
                }
                out += &format!(
                    "- #{} [{:?}] P{} {} ({}){}{}{}\n",
                    t.id, t.status, t.priority, t.title, t.role,
                    if t.due.is_empty() { String::new() } else { format!(" due {}", t.due) },
                    if t.pr_url.is_empty() { String::new() } else { format!(" PR {}", t.pr_url) },
                    if t.human_ask.is_empty() { String::new() } else { format!(" — POTŘEBUJE FELIXE: {}", t.human_ask) },
                );
            }
        }
        out
    }
}

fn or_dash(s: &str) -> &str {
    if s.is_empty() { "?" } else { s }
}

#[derive(Clone)]
pub struct Shared(pub Arc<Mutex<Board>>);

impl Shared {
    pub fn load() -> Self {
        let path = crate::config::state_dir().join("board.json");
        let b = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Shared(Arc::new(Mutex::new(b)))
    }

    /// Run `f` with the board locked, then persist atomically.
    pub fn with<R>(&self, f: impl FnOnce(&mut Board) -> R) -> R {
        let mut b = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut b);
        let dir = crate::config::state_dir();
        let tmp = dir.join("board.json.tmp");
        if let Ok(s) = serde_json::to_string_pretty(&*b) {
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, dir.join("board.json"));
            }
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_and_pick() {
        let mut b = Board::default();
        assert!(b.add_task("a", "Login", "", 2, "dev", "").is_some());
        assert!(b.add_task("a", " login ", "", 1, "dev", "").is_none());
        let late = b.add_task("a", "Later", "", 1, "dev", "2027-01-01").unwrap();
        let soon = b.add_task("b", "Soon", "", 5, "dev", "2026-12-01").unwrap();
        let allowed = vec!["a".to_string(), "b".to_string()];
        assert_eq!(b.pick_next(&allowed), Some(soon));
        b.task_mut(soon).unwrap().status = Status::InProgress;
        // project b busy → next due in a
        assert_eq!(b.pick_next(&allowed), Some(late));
        assert_eq!(b.pick_next(&["c".to_string()]), None);
    }
}
