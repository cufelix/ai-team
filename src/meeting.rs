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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Per-start secret embedded in the served page; required on every /api call.
/// Cross-site pages can't read it or send the custom header without a CORS preflight we never answer.
static TOKEN: OnceLock<String> = OnceLock::new();

fn random_hex() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).expect("/dev/urandom");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn token() -> &'static str {
    TOKEN.get_or_init(random_hex)
}

/// A merge/deploy proposed by the team. Runs only when Felix confirms this exact target.
#[derive(Clone)]
struct Pending {
    nonce: String,
    kind: &'static str, // "merge" | "deploy"
    project: String,
    pr_url: String,
    label: String,
    at: Instant,
}
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
const PENDING_TTL: Duration = Duration::from_secs(300);

async fn guard(
    State(port): State<u16>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::StatusCode;
    let host = req.headers().get("host").and_then(|h| h.to_str().ok()).unwrap_or("");
    // DNS-rebinding protection: only our own loopback origin.
    if host != format!("localhost:{port}") && host != format!("127.0.0.1:{port}") {
        return StatusCode::FORBIDDEN.into_response();
    }
    if req.uri().path().starts_with("/api/") && req.headers().get("x-ai-team").and_then(|h| h.to_str().ok()) != Some(token()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}

pub fn router(app: Arc<App>) -> Router {
    let port = app.cfg.server.port;
    Router::new()
        .route("/", get(|| async { Html(include_str!("../web/index.html").replace("__AI_TEAM_TOKEN__", token())) }))
        .route("/api/confirm", post(confirm))
        .route("/api/state", get(state))
        .route("/api/talk", post(talk))
        .route("/api/tick", post(|State(a): State<Arc<App>>| async move {
            tokio::spawn(async move { let _ = orchestrator::tick(&a).await; });
            Json(json!({"ok": true}))
        }))
        .with_state(app)
        .layer(axum::middleware::from_fn_with_state(port, guard))
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
- merge_pr {{project,pr}} a deploy {{project}} — jen navrhne akci; Felix ji pak musí potvrdit (systém mu ukáže přesný cíl).
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

    let mut done = Vec::new();
    // Spoken approval confirms only the action already shown on screen (bound target), never a new one.
    if approves(&text) {
        if let Some(p) = take_pending(None) {
            done.push(execute(app, p).await);
        }
    }
    for a in v["actions"].as_array().into_iter().flatten() {
        done.push(apply(app, a).await);
    }
    let pending = PENDING.lock().ok().and_then(|g| g.clone()).map(|p| json!({"nonce": p.nonce, "label": p.label}));
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
    Ok(json!({"transcript": text, "replies": replies, "actions": done, "pending": pending}))
}

/// Server-side guard for merge/deploy: Felix's own words must contain an explicit approval.
fn approves(text: &str) -> bool {
    let t = text.to_lowercase();
    if ["neschval", "nemerg", "nenasa", "nedeploy", "počkej", "ještě ne"].iter().any(|k| t.contains(k)) {
        return false;
    }
    ["schval", "mergn", "merguj", "nasaď", "nasad", "deployn", "deployuj", "pushni to", "pusť to ven"].iter().any(|k| t.contains(k))
}

async fn apply(app: &Arc<App>, a: &Value) -> String {
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
        "merge_pr" => stage_merge(app, &project, &s("pr")),
        "deploy" => stage_deploy(app, &project),
        other => Err(anyhow::anyhow!("neznámá akce {other}")),
    };
    let msg = res.unwrap_or_else(|e| format!("✗ {e:#}"));
    app.board.with(|b| b.log("systém", msg.clone()));
    msg
}

fn stage(kind: &'static str, project: &str, pr_url: String, label: String) -> anyhow::Result<String> {
    let nonce = random_hex();
    let msg = format!("čeká na potvrzení: {label} (řekni „schvaluju“ nebo klikni)");
    *PENDING.lock().map_err(|_| anyhow::anyhow!("lock"))? = Some(Pending { nonce, kind, project: project.into(), pr_url, label, at: Instant::now() });
    Ok(msg)
}

/// Only PRs the team opened for this project and that await review can be merged.
fn stage_merge(app: &App, project: &str, pr: &str) -> anyhow::Result<String> {
    let num = pr.rsplit('/').next().unwrap_or("").trim_start_matches('#');
    anyhow::ensure!(!num.is_empty() && num.chars().all(|c| c.is_ascii_digit()), "neplatné číslo PR '{pr}'");
    let url = app.board.with(|b| {
        b.tasks.iter()
            .find(|t| t.project == project && matches!(t.status, Status::Review | Status::NeedsHuman) && t.pr_url.ends_with(&format!("/pull/{num}")))
            .map(|t| t.pr_url.clone())
    }).ok_or_else(|| anyhow::anyhow!("PR {num} není mezi PR týmu v projektu {project}"))?;
    stage("merge", project, url, format!("merge PR {num} v projektu {project}"))
}

fn stage_deploy(app: &App, project: &str) -> anyhow::Result<String> {
    let p = app.cfg.project(project).ok_or_else(|| anyhow::anyhow!("neznámý projekt"))?;
    anyhow::ensure!(!p.deploy_cmd.trim().is_empty(), "projekt {project} nemá deploy_cmd");
    stage("deploy", project, String::new(), format!("deploy projektu {project}"))
}

/// Takes the pending action if fresh and (when given) the nonce matches. One approval = one action.
fn take_pending(nonce: Option<&str>) -> Option<Pending> {
    let mut g = PENDING.lock().ok()?;
    let p = g.as_ref()?;
    if p.at.elapsed() > PENDING_TTL || nonce.is_some_and(|n| n != p.nonce) {
        return None;
    }
    g.take()
}

async fn execute(app: &Arc<App>, p: Pending) -> String {
    let res = match p.kind {
        "merge" => orchestrator::merge_pr(app, &p.project, &p.pr_url).await,
        _ => orchestrator::deploy(app, &p.project).await,
    };
    let msg = res.unwrap_or_else(|e| format!("✗ {e:#}"));
    app.board.with(|b| b.log("systém", format!("schváleno: {} → {msg}", p.label)));
    msg
}

async fn confirm(State(app): State<Arc<App>>, Json(body): Json<Value>) -> impl IntoResponse {
    match take_pending(Some(body["nonce"].as_str().unwrap_or("-"))) {
        Some(p) => Json(json!({"result": execute(&app, p).await})),
        None => Json(json!({"error": "žádná platná akce k potvrzení (vypršela?)"})),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn approval_guard() {
        assert!(super::approves("Jo, schvaluju, mergni to"));
        assert!(super::approves("nasaď to na produkci"));
        assert!(!super::approves("co je v tom PR?"));
        assert!(!super::approves("neschvaluju, ještě ne"));
        assert!(super::take_pending(None).is_none());
        super::stage("deploy", "x", String::new(), "deploy x".into()).unwrap();
        assert!(super::take_pending(Some("wrong")).is_none());
        assert!(super::take_pending(None).is_some());
        assert!(super::take_pending(None).is_none()); // single use
    }
}
