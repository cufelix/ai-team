//! The autonomous loop: manager plans → dev agents work in git worktrees → PR → Felix reviews.
//! Merge/deploy are NOT done here; only after Felix's spoken approval (see meeting.rs).
use crate::board::{Shared, Status};
use crate::claude::{self, RateLimited};
use crate::config::{Config, Project};
use crate::{gcal, notify};
use anyhow::Context;
use chrono::{Datelike, Duration, Local, TimeZone};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct App {
    pub cfg: Config,
    pub board: Shared,
    pub http: reqwest::Client,
}

pub async fn run_forever(app: Arc<App>) {
    loop {
        if let Err(e) = tick(&app).await {
            app.board.with(|b| b.log("orchestrátor", format!("chyba ticku: {e:#}")));
        }
        tokio::time::sleep(std::time::Duration::from_secs(app.cfg.budget.tick_minutes * 60)).await;
    }
}

pub async fn tick(app: &Arc<App>) -> anyhow::Result<()> {
    sync_prs(app).await;
    ensure_standups(app).await;
    if let Some(reason) = budget_block(app).await {
        app.board.with(|b| b.log("orchestrátor", format!("pauza: {reason}")));
        return Ok(());
    }
    // One planning run per tick at most: planning is the cheap part, but still tokens.
    if let Some(p) = app.cfg.projects.iter().find(|p| plan_due(app, p)) {
        if let Err(e) = plan(app, p).await {
            handle_err(app, &e, &format!("plánování {}", p.id));
            return Ok(());
        }
    }
    let allowed: Vec<String> = app.cfg.projects.iter().filter(|p| p.autonomous && p.path.join(".git").exists()).map(|p| p.id.clone()).collect();
    let running = app.board.with(|b| b.tasks.iter().filter(|t| t.status == Status::InProgress).count());
    for _ in running..app.cfg.budget.max_concurrent {
        let picked = app.board.with(|b| {
            if b.runs_today() >= app.cfg.budget.max_dev_runs_per_day {
                return None;
            }
            let id = b.pick_next(&allowed)?;
            b.bump_run_counter();
            let t = b.task_mut(id)?;
            t.status = Status::InProgress;
            t.attempts += 1;
            t.updated = Local::now();
            Some(id)
        });
        let Some(id) = picked else { break };
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = develop(&app, id).await {
                handle_err(&app, &e, &format!("úkol #{id}"));
                app.board.with(|b| {
                    if let Some(t) = b.task_mut(id) {
                        if t.status == Status::InProgress {
                            t.status = Status::Todo;
                            t.result = format!("selhalo: {}", claude::truncate(&format!("{e:#}"), 300));
                        }
                    }
                });
            }
        });
    }
    Ok(())
}

fn handle_err(app: &App, e: &anyhow::Error, ctx: &str) {
    let limited = e.downcast_ref::<RateLimited>().is_some();
    app.board.with(|b| {
        if limited {
            b.paused_until = Some(Local::now() + Duration::minutes(60));
            b.log("orchestrátor", format!("{ctx}: došel limit, pauza 60 min"));
        } else {
            b.log("orchestrátor", format!("{ctx}: {}", claude::truncate(&format!("{e:#}"), 300)));
        }
    });
}

/// Returns why we must not spend tokens right now, if anything.
async fn budget_block(app: &App) -> Option<String> {
    let now = Local::now();
    if let Some(until) = app.board.with(|b| b.paused_until) {
        if until > now {
            return Some(format!("do {}", until.format("%H:%M")));
        }
    }
    if app.cfg.budget.quiet_hours.contains(&chrono::Timelike::hour(&now)) {
        return Some("quiet hours".into());
    }
    let cost = active_block_cost().await?;
    (cost > app.cfg.budget.pause_if_block_cost_usd).then(|| format!("aktivní 5h blok ${cost:.1} > ${}", app.cfg.budget.pause_if_block_cost_usd))
}

/// Cost of the current 5h usage block per ccusage (None if ccusage unavailable).
pub async fn active_block_cost() -> Option<f64> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new("ccusage").args(["blocks", "--active", "--json", "--offline"]).output(),
    )
    .await
    .ok()?
    .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v["blocks"].as_array()?.iter().find(|b| b["isActive"] == true)?["costUSD"].as_f64()
}

fn plan_due(app: &App, p: &Project) -> bool {
    if !p.path.exists() {
        return false;
    }
    app.board.with(|b| {
        let st = b.projects.get(&p.id);
        let open = b.open_tasks(&p.id).iter().filter(|t| t.status == Status::Todo).count();
        match st.and_then(|s| s.last_plan) {
            None => true,
            Some(t) => open == 0 && Local::now() - t > Duration::hours(1) || Local::now() - t > Duration::hours(app.cfg.budget.plan_every_hours),
        }
    })
}

pub async fn plan(app: &App, p: &Project) -> anyhow::Result<()> {
    let sef = app.cfg.role("sef").or(app.cfg.roles.first()).context("no roles")?;
    let roles: Vec<&str> = app.cfg.roles.iter().map(|r| r.id.as_str()).collect();
    let (snapshot, deadline) = app.board.with(|b| {
        (b.snapshot(Some(&p.id)), b.projects.get(&p.id).map(|s| s.deadline.clone()).unwrap_or_default())
    });
    let deadline = if deadline.is_empty() { p.deadline.clone() } else { deadline };
    let prompt = format!(
        "Jsi {name}, {title} AI týmu. Projekt: {pname}. Poznámky: {notes}\nDeadline: {dl}. Dnes je {today}.\n\
Aktuální board:\n{snapshot}\n\
Rychle zjisti stav projektu (CLAUDE.md, PROGRESS.md / docs, `git log --oneline -15`, `gh pr list`). Nečti zbytečně celé soubory, šetři tokeny.\n\
Pak vrať POUZE JSON:\n\
{{\"summary\":\"2-3 věty o stavu a riziku vůči deadlinu\",\"deadline\":\"YYYY-MM-DD nebo prázdné, pokud nevíš\",\
\"tasks\":[{{\"title\":\"...\",\"description\":\"co přesně udělat + kritérium hotovo + jak ověřit\",\"priority\":1-5,\"role\":\"{roles}\",\"due\":\"YYYY-MM-DD nebo prázdné\"}}],\
\"drop\":[id úkolů, které už nedávají smysl]}}\n\
Max 4 nové úkoly, každý zvládne jeden agent do 40 minut, nejdůležitější pro deadline první. Neduplikuj otevřené úkoly.",
        name = sef.name, title = sef.title, pname = p.name, notes = p.notes, dl = if deadline.is_empty() { "neznámý" } else { &deadline },
        today = Local::now().format("%Y-%m-%d %A"), roles = roles.join("|"),
    );
    app.board.with(|b| b.log(&sef.name, format!("plánuju {}", p.name)));
    let out = claude::run(claude::Run {
        cwd: &p.path, prompt, model: &app.cfg.budget.plan_model, system: None, resume: None, dev_mode: false,
        timeout: std::time::Duration::from_secs(600),
    })
    .await?;
    let v = claude::last_json(&out.text).with_context(|| format!("plán bez JSON: {}", claude::truncate(&out.text, 200)))?;
    app.board.with(|b| {
        let st = b.projects.entry(p.id.clone()).or_default();
        st.last_plan = Some(Local::now());
        st.summary = v["summary"].as_str().unwrap_or_default().to_string();
        if st.deadline.is_empty() {
            st.deadline = if p.deadline.is_empty() { v["deadline"].as_str().unwrap_or_default().to_string() } else { p.deadline.clone() };
        }
        for id in v["drop"].as_array().into_iter().flatten().filter_map(|x| x.as_u64()) {
            if let Some(t) = b.task_mut(id) {
                if t.project == p.id && t.status == Status::Todo {
                    t.status = Status::Dropped;
                }
            }
        }
        let mut added = 0;
        for t in v["tasks"].as_array().into_iter().flatten().take(4) {
            let title = t["title"].as_str().unwrap_or_default();
            if title.is_empty() {
                continue;
            }
            let role = t["role"].as_str().filter(|r| roles.contains(r)).unwrap_or("dev");
            if b.add_task(&p.id, title, t["description"].as_str().unwrap_or_default(), t["priority"].as_u64().unwrap_or(3) as u8, role, t["due"].as_str().unwrap_or_default()).is_some() {
                added += 1;
            }
        }
        b.log(&sef.name, format!("{}: +{added} úkolů. {}", p.name, b.projects[&p.id].summary));
    });
    Ok(())
}

async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    // Hooks off: agents can write .git/hooks, the orchestrator must not run them with push credentials.
    let out = tokio::process::Command::new("git").current_dir(dir).args(["-c", "core.hooksPath=/dev/null"]).args(args).output().await?;
    anyhow::ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn gh(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("gh").current_dir(dir).args(args).output().await?;
    anyhow::ensure!(out.status.success(), "gh {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        let c = match c {
            'á' => 'a', 'č' => 'c', 'ď' => 'd', 'é' | 'ě' => 'e', 'í' => 'i', 'ň' => 'n', 'ó' => 'o', 'ř' => 'r', 'š' => 's', 'ť' => 't', 'ú' | 'ů' => 'u', 'ý' => 'y', 'ž' => 'z',
            c if c.is_ascii_alphanumeric() => c,
            _ => '-',
        };
        if !(c == '-' && (out.is_empty() || out.ends_with('-'))) {
            out.push(c);
        }
    }
    out.trim_end_matches('-').chars().take(32).collect::<String>().trim_end_matches('-').to_string()
}

fn worktree_dir(project: &str, id: u64) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("projects/.ai-team-worktrees").join(project).join(id.to_string())
}

async fn default_base(repo: &Path) -> String {
    let _ = git(repo, &["fetch", "-q", "origin"]).await;
    git(repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).await.unwrap_or_else(|_| "HEAD".into())
}

async fn develop(app: &Arc<App>, id: u64) -> anyhow::Result<()> {
    let task = app.board.with(|b| b.task_mut(id).cloned()).context("task gone")?;
    let p = app.cfg.project(&task.project).context("project gone")?.clone();
    let role = app.cfg.role(&task.role).or(app.cfg.role("dev")).context("no dev role")?.clone();
    let base = default_base(&p.path).await;
    let branch = format!("team/{}-{}", id, slug(&task.title));
    let wt = worktree_dir(&p.id, id);
    if wt.exists() {
        let _ = git(&p.path, &["worktree", "remove", "--force", wt.to_str().unwrap_or_default()]).await;
    }
    std::fs::create_dir_all(wt.parent().context("wt parent")?)?;
    if git(&p.path, &["worktree", "add", "-q", "-b", &branch, wt.to_str().unwrap_or_default(), &base]).await.is_err() {
        git(&p.path, &["worktree", "add", "-q", wt.to_str().unwrap_or_default(), &branch]).await?; // branch from a previous attempt
    }
    app.board.with(|b| {
        if let Some(t) = b.task_mut(id) { t.branch = branch.clone(); }
        b.log(&role.name, format!("začínám #{id} {} ({})", task.title, p.name));
    });
    let system = format!(
        "Jsi {} — {} v autonomním AI týmu Felixe. {}\nPracuješ samostatně ve vlastním git worktree na větvi {branch}. \
Felix není k dispozici; neptej se, rozhodni rozumně sám. Šetři tokeny: čti cíleně, žádné zbytečné průzkumy. \
Na nezávislé části můžeš použít subagenty (Agent tool). Pokud chybí závislosti (node_modules apod.), nainstaluj je (pnpm/npm/cargo). \
Nikdy nepushuj, nemerguj, nedeployuj — to dělá orchestrátor po Felixově schválení.",
        role.name, role.title, role.persona
    );
    let prompt = format!(
        "Úkol #{id} pro projekt {}: {}\n\n{}\n\nPostup: implementuj, spusť relevantní testy/lint/build, oprav chyby, \
průběžně commituj (conventional commits) — nové soubory přidej do commitu sám, necommitnuté nové soubory se zahodí. Pokud něco bez Felixe nejde (klíče, rozhodnutí, ruční test na zařízení), udělej maximum a popiš to.\n\
Na úplný konec odpovědi dej POUZE tento JSON:\n\
{{\"status\":\"done|blocked|needs_human\",\"summary\":\"co je hotovo (2-4 věty)\",\"test_instructions\":\"jak to má Felix ručně ověřit\",\"human_ask\":\"co přesně potřebuješ od Felixe, jinak prázdné\"}}",
        p.name, task.title, task.description
    );
    let res = claude::run(claude::Run {
        cwd: &wt, prompt, model: &app.cfg.budget.dev_model, system: Some(system), resume: None, dev_mode: true,
        timeout: std::time::Duration::from_secs(app.cfg.budget.dev_timeout_minutes * 60),
    })
    .await;
    // Never lose work: commit anything the agent left behind.
    if !git(&wt, &["status", "--porcelain"]).await.unwrap_or_default().is_empty() {
        let _ = git(&wt, &["add", "-u"]).await; // tracked files only: untracked junk (caches, tool dirs) stays out
        let _ = git(&wt, &["commit", "-q", "-m", &format!("wip: #{id} {} (auto-commit)", task.title)]).await;
    }
    let commits: u32 = git(&wt, &["rev-list", "--count", &format!("{base}..HEAD")]).await.unwrap_or_default().parse().unwrap_or(0);
    let out = res?;
    let v = claude::last_json(&out.text).unwrap_or_else(|| serde_json::json!({"status":"blocked","summary": claude::truncate(&out.text, 400)}));
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let (status, summary, tests, ask) = (s("status"), s("summary"), s("test_instructions"), s("human_ask"));

    let mut pr_url = String::new();
    if commits > 0 {
        match publish_pr(&p.path, &wt, &branch, &base, id, &task.title, &summary, &tests).await {
            Ok(u) => pr_url = u,
            Err(e) => app.board.with(|b| b.log("orchestrátor", format!("#{id} PR nevytvořen: {e:#}"))),
        }
    }
    let _ = git(&p.path, &["worktree", "remove", "--force", wt.to_str().unwrap_or_default()]).await;
    if commits == 0 {
        let _ = git(&p.path, &["branch", "-D", &branch]).await;
    }
    let needs_human = status == "needs_human" || !ask.is_empty();
    let new_status = match (commits > 0, needs_human, status.as_str()) {
        (_, true, _) => Status::NeedsHuman,
        (true, _, _) => Status::Review,
        (false, _, "done") => Status::Done,
        _ => Status::Todo,
    };
    app.board.with(|b| {
        if let Some(t) = b.task_mut(id) {
            t.status = new_status.clone();
            t.result = format!("{summary}\nTest: {tests}");
            t.human_ask = ask.clone();
            t.pr_url = pr_url.clone();
            t.updated = Local::now();
        }
        b.log(&role.name, format!("#{id} → {new_status:?}. {summary}{}", if pr_url.is_empty() { String::new() } else { format!(" PR: {pr_url}") }));
    });
    let room = &app.cfg.server.public_url;
    match new_status {
        Status::NeedsHuman => call_felix(app, &format!("{}: potřebuju tě", p.name), &format!("#{id} {}: {ask}", task.title)).await,
        Status::Review => notify::felix(&app.cfg, &app.http, &format!("{}: PR k otestování", p.name), &format!("#{id} {}\n{tests}", task.title), if pr_url.is_empty() { room } else { &pr_url }).await,
        _ => {}
    }
    Ok(())
}

async fn publish_pr(repo: &Path, wt: &Path, branch: &str, base: &str, id: u64, title: &str, summary: &str, tests: &str) -> anyhow::Result<String> {
    git(wt, &["push", "-q", "-u", "origin", branch]).await?;
    let base_branch = base.trim_start_matches("origin/");
    let body = format!("{summary}\n\n## Jak otestovat\n{tests}\n\n_AI tým, úkol #{id}. Merge jen po Felixově schválení._");
    match gh(repo, &["pr", "create", "--head", branch, "--base", base_branch, "--title", &format!("#{id} {title}"), "--body", &body]).await {
        Ok(url) => Ok(url.lines().last().unwrap_or_default().to_string()),
        Err(_) => gh(repo, &["pr", "view", branch, "--json", "url", "-q", ".url"]).await,
    }
}

/// "Call" Felix: ad-hoc meeting in ~10 minutes + push notification.
pub async fn call_felix(app: &App, title: &str, body: &str) {
    let room = &app.cfg.server.public_url;
    let start = (Local::now() + Duration::minutes(10)).fixed_offset();
    let (created, link) = gcal::schedule(&app.http, &format!("AI tým: {title}"), &format!("{body}\n\nMístnost: {room}"), room, start, 15).await;
    let body = if created { body.to_string() } else { format!("{body}\nPřidat do kalendáře: {link}") };
    notify::felix(&app.cfg, &app.http, title, &body, room).await;
    app.board.with(|b| b.log("orchestrátor", format!("volám Felixe: {title}")));
}

/// Free (no tokens): reflect merged/closed PRs on the board.
async fn sync_prs(app: &App) {
    let review: Vec<(u64, String, PathBuf)> = app.board.with(|b| {
        b.tasks.iter().filter(|t| matches!(t.status, Status::Review | Status::NeedsHuman) && !t.pr_url.is_empty())
            .filter_map(|t| Some((t.id, t.pr_url.clone(), app.cfg.project(&t.project)?.path.clone()))).collect()
    });
    for (id, url, path) in review {
        if let Ok(state) = gh(&path, &["pr", "view", &url, "--json", "state", "-q", ".state"]).await {
            let st = match state.as_str() { "MERGED" => Status::Done, "CLOSED" => Status::Dropped, _ => continue };
            app.board.with(|b| if let Some(t) = b.task_mut(id) { t.status = st; t.updated = Local::now(); });
        }
    }
}

/// Creates the mandatory standups for the next 7 days (only when Google Calendar is connected).
async fn ensure_standups(app: &App) {
    if !gcal::configured() {
        return;
    }
    let Ok(off) = app.cfg.standup.timezone_offset.parse::<chrono::FixedOffset>() else { return };
    let Ok(time) = chrono::NaiveTime::parse_from_str(&app.cfg.standup.time, "%H:%M") else { return };
    for d in 0..7 {
        let day = Local::now().date_naive() + Duration::days(d);
        let wd = format!("{:?}", day.weekday());
        if !app.cfg.standup.days.iter().any(|x| x.eq_ignore_ascii_case(&wd)) {
            continue;
        }
        let key = format!("standup-{day}");
        if app.board.with(|b| b.calendar_events.contains(&key)) {
            continue;
        }
        let Some(start) = off.from_local_datetime(&day.and_time(time)).single() else { continue };
        if start < Local::now().fixed_offset() {
            continue;
        }
        let room = &app.cfg.server.public_url;
        let agenda = app.board.with(|b| b.snapshot(None));
        let (ok, _) = gcal::schedule(&app.http, "AI tým: standup (povinný)", &format!("Připoj se: {room}\n\nStav:\n{}", claude::truncate(&agenda, 3000)), room, start, app.cfg.standup.minutes).await;
        if ok {
            app.board.with(|b| { b.calendar_events.push(key.clone()); b.log("orchestrátor", format!("standup v kalendáři: {day}")); });
        }
    }
}

// --- actions Felix approves verbally in the meeting ---

pub async fn merge_pr(app: &App, project: &str, pr: &str) -> anyhow::Result<String> {
    let p = app.cfg.project(project).context("neznámý projekt")?;
    anyhow::ensure!(!pr.is_empty() && !pr.starts_with('-'), "neplatné PR");
    gh(&p.path, &["pr", "merge", pr, "--squash", "--delete-branch"]).await?;
    app.board.with(|b| {
        for t in b.tasks.iter_mut().filter(|t| t.project == project && (t.pr_url == pr || t.pr_url.ends_with(&format!("/pull/{pr}")))) {
            t.status = Status::Done;
            t.updated = Local::now();
        }
        b.log("orchestrátor", format!("{project}: PR {pr} mergnut (schválil Felix)"));
    });
    Ok(format!("PR {pr} mergnut"))
}

pub async fn deploy(app: &App, project: &str) -> anyhow::Result<String> {
    let p = app.cfg.project(project).context("neznámý projekt")?;
    anyhow::ensure!(!p.deploy_cmd.trim().is_empty(), "projekt {project} nemá v team.toml deploy_cmd");
    let base = default_base(&p.path).await;
    let wt = worktree_dir(&p.id, 0).with_file_name("deploy");
    let _ = git(&p.path, &["worktree", "remove", "--force", wt.to_str().unwrap_or_default()]).await;
    git(&p.path, &["worktree", "add", "-q", "--detach", wt.to_str().unwrap_or_default(), &base]).await?;
    let out = tokio::process::Command::new("bash").arg("-lc").arg(&p.deploy_cmd).current_dir(&wt).output().await?;
    let _ = git(&p.path, &["worktree", "remove", "--force", wt.to_str().unwrap_or_default()]).await;
    let tail = claude::truncate(&String::from_utf8_lossy(&out.stdout).lines().rev().take(5).collect::<Vec<_>>().join(" | "), 300);
    app.board.with(|b| b.log("orchestrátor", format!("{project}: deploy {} {tail}", if out.status.success() { "OK" } else { "SELHAL" })));
    anyhow::ensure!(out.status.success(), "deploy selhal: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""));
    Ok(format!("deploy {project} OK"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn slug_strips_diacritics() {
        assert_eq!(super::slug("Přidat přihlášení / Google!"), "pridat-prihlaseni-google");
        assert_eq!(super::slug("---"), "");
    }
}
