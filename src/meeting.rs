//! Meeting room: browser mic → ElevenLabs STT → team (claude -p, resumed per day) → ElevenLabs TTS per role.
use crate::board::Status;
use crate::orchestrator::{self, App};
use crate::{claude, voice};
use axum::extract::{Multipart, State};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use chrono::Local;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("../web/index.html")) }))
        .route("/api/state", get(state))
        .route("/api/talk", post(talk))
        .route("/api/tick", post(|State(a): State<Arc<App>>| async move {
            tokio::spawn(async move { let _ = orchestrator::tick(&a).await; });
            Json(json!({"ok": true}))
        }))
        .with_state(app)
}

async fn state(State(app): State<Arc<App>>) -> impl IntoResponse {
    let cost = orchestrator::active_block_cost().await;
    let b = app.board.0.lock().map(|b| b.clone()).unwrap_or_default();
    Json(json!({
        "roles": app.cfg.roles,
        "projects": app.cfg.projects,
        "board": b,
        "runs_today": b.runs_today(),
        "budget": {"max_runs": app.cfg.budget.max_dev_runs_per_day, "block_cost": cost, "pause_at": app.cfg.budget.pause_if_block_cost_usd},
        "calendar": crate::gcal::configured(),
    }))
}

fn system_prompt(app: &App) -> String {
    let roster: String = app.cfg.roles.iter().map(|r| format!("- {} ({}, id \"{}\"): {}\n", r.name, r.title, r.id, r.persona)).collect();
    let projects: String = app.cfg.projects.iter().map(|p| format!("- {} (id \"{}\", {}): {}\n", p.name, p.id, if p.autonomous { "autonomní" } else { "jen plánování" }, p.notes)).collect();
    format!(
"Jsi celý AI vývojový tým Felixe na hlasovém meetingu. Mluví se česky, přirozeně a VELMI STRUČNĚ (mluvené slovo: max 2 věty na osobu, celkem do 70 slov, žádný markdown, žádné odrážky, čísla PR říkej slovy jako \"pull request dvanáct\").
Tým:
{roster}Projekty:
{projects}Odpovídá ten, koho se to týká (obvykle 1, max 2 lidi). Felix se může doptat. Šéf vede meeting, shrnuje a hlídá deadliny.
Primárně vycházej z [Aktuální stav]. Repozitáře v ~/projects čti (Grep/git log) jen když Felix chce detail, který tam není — Felix čeká na odpověď.
Akce (provede je systém):
- add_task {{project,title,description,priority 1-5,role,due}}
- set_deadline {{project,date YYYY-MM-DD}}
- set_priority {{task,priority}} / drop_task {{task}}
- pause_project {{project,paused bool}}
- replan {{project}} — šéf přeplánuje projekt
- run_now {{}} — hned spustit práci
- merge_pr {{project,pr}} a deploy {{project}} — POUZE když to Felix v této větě výslovně schválil (\"schvaluju\", \"mergni\", \"nasaď\", \"jo, deployni\"). Jinak se nejdřív zeptej.
Odpověz VŽDY jen JSON: {{\"replies\":[{{\"role\":\"<id>\",\"text\":\"...\"}}],\"actions\":[{{\"type\":\"...\", ...}}]}}"
    )
}

async fn talk(State(app): State<Arc<App>>, mut mp: Multipart) -> impl IntoResponse {
    match talk_inner(&app, &mut mp).await {
        Ok(v) => Json(v),
        Err(e) => Json(json!({"error": format!("{e:#}")})),
    }
}

async fn talk_inner(app: &Arc<App>, mp: &mut Multipart) -> anyhow::Result<Value> {
    let mut text = String::new();
    while let Some(field) = mp.next_field().await? {
        match field.name() {
            Some("text") => text = field.text().await?,
            Some("audio") => {
                let mime = field.content_type().unwrap_or("audio/webm").to_string();
                let bytes = field.bytes().await?;
                anyhow::ensure!(bytes.len() < 20 << 20, "audio too large");
                text = voice::stt(&app.http, bytes.to_vec(), &mime).await?;
            }
            _ => {}
        }
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        return Ok(json!({"transcript": "", "replies": []}));
    }
    let today = Local::now().date_naive();
    let (snapshot, resume) = app.board.with(|b| {
        b.log("Felix", text.clone());
        (b.snapshot(None), b.meeting_session.as_ref().filter(|(d, _)| *d == today).map(|(_, s)| s.clone()))
    });
    let prompt = format!("[Aktuální stav {}]\n{snapshot}\n[Felix říká]: {text}", Local::now().format("%a %d.%m. %H:%M"));
    let home = std::env::var("HOME").unwrap_or_default();
    let out = claude::run(claude::Run {
        cwd: std::path::Path::new(&format!("{home}/projects")),
        prompt,
        model: &app.cfg.budget.meeting_model,
        system: Some(system_prompt(app)),
        resume,
        dev_mode: false,
        timeout: std::time::Duration::from_secs(180),
    })
    .await?;
    app.board.with(|b| b.meeting_session = Some((today, out.session_id.clone())));
    let v = claude::last_json(&out.text).unwrap_or_else(|| json!({"replies":[{"role":"sef","text": out.text}]}));

    let approved = approves(&text);
    let mut done = Vec::new();
    for a in v["actions"].as_array().into_iter().flatten() {
        done.push(apply(app, a, approved).await);
    }
    // TTS for all lines in parallel, then keep the original order.
    let mut jobs = Vec::new();
    for r in v["replies"].as_array().into_iter().flatten() {
        let Some(role) = app.cfg.role(r["role"].as_str().unwrap_or("sef")).or(app.cfg.roles.first()).cloned() else { continue };
        let line = r["text"].as_str().unwrap_or_default().to_string();
        if line.is_empty() {
            continue;
        }
        app.board.with(|b| b.log(&role.name, line.clone()));
        let http = app.http.clone();
        jobs.push(tokio::spawn(async move {
            let audio = match voice::tts(&http, &role.voice, &line).await {
                Ok(mp3) => base64::engine::general_purpose::STANDARD.encode(mp3),
                Err(e) => { eprintln!("tts: {e:#}"); String::new() }
            };
            json!({"role": role.id, "name": role.name, "title": role.title, "text": line, "audio": audio})
        }));
    }
    let mut replies = Vec::new();
    for j in jobs {
        if let Ok(r) = j.await { replies.push(r); }
    }
    Ok(json!({"transcript": text, "replies": replies, "actions": done}))
}

/// Server-side guard for merge/deploy: Felix's own words must contain an explicit approval.
fn approves(text: &str) -> bool {
    let t = text.to_lowercase();
    if ["neschval", "nemerg", "nenasa", "nedeploy", "počkej", "ještě ne"].iter().any(|k| t.contains(k)) {
        return false;
    }
    ["schval", "mergn", "merguj", "nasaď", "nasad", "deployn", "deployuj", "pushni to", "pusť to ven"].iter().any(|k| t.contains(k))
}

async fn apply(app: &Arc<App>, a: &Value, approved: bool) -> String {
    let s = |k: &str| a[k].as_str().map(str::to_string).or_else(|| a[k].as_u64().map(|n| n.to_string())).unwrap_or_default();
    let project = s("project");
    let res: anyhow::Result<String> = match a["type"].as_str().unwrap_or_default() {
        "add_task" if app.cfg.project(&project).is_some() => Ok(app.board.with(|b| {
            match b.add_task(&project, &s("title"), &s("description"), a["priority"].as_u64().unwrap_or(2) as u8, a["role"].as_str().unwrap_or("dev"), &s("due")) {
                Some(id) => format!("úkol #{id} přidán"),
                None => "úkol už existuje".into(),
            }
        })),
        "set_deadline" if app.cfg.project(&project).is_some() => Ok(app.board.with(|b| {
            b.projects.entry(project.clone()).or_default().deadline = s("date");
            format!("deadline {project} = {}", s("date"))
        })),
        "set_priority" | "drop_task" => {
            let id = a["task"].as_u64().unwrap_or(0);
            app.board.with(|b| match b.task_mut(id) {
                Some(t) if a["type"] == "drop_task" => { t.status = Status::Dropped; Ok(format!("#{id} zrušen")) }
                Some(t) => { t.priority = a["priority"].as_u64().unwrap_or(3).clamp(1, 5) as u8; Ok(format!("#{id} priorita {}", t.priority)) }
                None => Err(anyhow::anyhow!("úkol #{id} neexistuje")),
            })
        }
        "pause_project" if app.cfg.project(&project).is_some() => Ok(app.board.with(|b| {
            let paused = a["paused"].as_bool().unwrap_or(true);
            b.projects.entry(project.clone()).or_default().paused = paused;
            format!("{project} {}", if paused { "pozastaven" } else { "běží" })
        })),
        "replan" => match app.cfg.project(&project).cloned() {
            Some(p) => {
                let app = app.clone();
                tokio::spawn(async move { let _ = orchestrator::plan(&app, &p).await; });
                Ok(format!("přeplánovávám {project}"))
            }
            None => Err(anyhow::anyhow!("neznámý projekt")),
        },
        "run_now" => {
            let app = app.clone();
            tokio::spawn(async move { let _ = orchestrator::tick(&app).await; });
            Ok("spouštím práci".into())
        }
        "merge_pr" if approved => orchestrator::merge_pr(app, &project, &s("pr")).await,
        "deploy" if approved => orchestrator::deploy(app, &project).await,
        "merge_pr" | "deploy" => Err(anyhow::anyhow!("zablokováno: chybí tvé výslovné schválení (řekni např. 'schvaluju merge')")),
        other => Err(anyhow::anyhow!("neznámá akce {other}")),
    };
    let msg = res.unwrap_or_else(|e| format!("✗ {e:#}"));
    app.board.with(|b| b.log("systém", msg.clone()));
    msg
}

#[cfg(test)]
mod tests {
    #[test]
    fn approval_guard() {
        assert!(super::approves("Jo, schvaluju, mergni to"));
        assert!(super::approves("nasaď to na produkci"));
        assert!(!super::approves("co je v tom PR?"));
        assert!(!super::approves("neschvaluju, ještě ne"));
    }
}
