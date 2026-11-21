mod board;
mod claude;
mod config;
mod gcal;
mod meeting;
mod notify;
mod orchestrator;
mod voice;

use std::sync::Arc;

const HELP: &str = "ai-team — autonomní AI vývojový tým
  serve [--idle] orchestrátor + meeting room (--idle = jen meeting, agenti nemakají)
  tick          jedno kolo plánování/práce a konec
  plan <id>     šéf hned přeplánuje projekt
  status        vypíše board
  auth-google   jednorázové přihlášení ke Google Kalendáři (state/google_client.json)
  call <text>   otestuje 'zavolání' Felixe (notifikace + kalendář)";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    config::load_dotenv();
    let cfg = config::Config::load(&config::root().join("team.toml"))?;
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(120)).build()?;
    let app = Arc::new(orchestrator::App { cfg, board: board::Shared::load(), http });
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => {
            let addr = format!("127.0.0.1:{}", app.cfg.server.port);
            // Tasks marked in-progress by a previous (killed) process will never finish → requeue.
            app.board.with(|b| {
                for t in b.tasks.iter_mut().filter(|t| t.status == board::Status::InProgress) {
                    t.status = board::Status::Todo;
                }
                b.log("orchestrátor", format!("start, meeting room http://{addr}"));
            });
            if !args.iter().any(|a| a == "--idle") {
                tokio::spawn(orchestrator::run_forever(app.clone()));
            }
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, meeting::router(app)).await?;
        }
        Some("tick") => {
            orchestrator::tick(&app).await?;
            // wait for spawned dev runs
            while app.board.with(|b| b.tasks.iter().any(|t| t.status == board::Status::InProgress)) {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        }
        Some("plan") => {
            let id = args.get(1).map(String::as_str).unwrap_or_default();
            let p = app.cfg.project(id).ok_or_else(|| anyhow::anyhow!("neznámý projekt '{id}'"))?.clone();
            orchestrator::plan(&app, &p).await?;
            println!("{}", app.board.with(|b| b.snapshot(Some(id))));
        }
        Some("status") => println!("{}", app.board.with(|b| b.snapshot(None))),
        Some("auth-google") => gcal::auth(&app.http).await?,
        Some("call") => orchestrator::call_felix(&app, "test", &args[1..].join(" ")).await,
        _ => println!("{HELP}"),
    }
    Ok(())
}
