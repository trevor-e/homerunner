mod config;
mod github;
mod runtime;
mod scheduler;
mod store;
mod web;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::Command as StdCommand;

const PLIST_LABEL: &str = "dev.highstorm.homerunner";

#[derive(Parser)]
#[command(name = "homerunner", version, about = "Warm pool of ephemeral self-hosted GitHub Actions runners")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
    /// Path to config.toml
    #[arg(long, global = true)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Supervisor + dashboard in the foreground
    Run,
    /// One-shot pool/runner summary from the dashboard API
    Status,
    /// Token, runtime, image, and per-repo access checks
    Doctor,
    /// Write + bootstrap the launchd agent
    Install,
}

fn config_path(cli: &Cli) -> PathBuf {
    cli.config
        .clone()
        .unwrap_or_else(|| config::expand_tilde("~/.config/homerunner/config.toml"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = config_path(&cli);

    if let Cmd::Install = cli.command {
        return install(&path);
    }

    let cfg = config::load(&path)?;
    match cli.command {
        Cmd::Run => run(cfg).await,
        Cmd::Status => status(cfg).await,
        Cmd::Doctor => doctor(cfg).await,
        Cmd::Install => unreachable!(),
    }
}

async fn run(cfg: config::Config) -> Result<()> {
    let github = github::GitHub::new(&cfg.auth_source);
    let store = store::Store::open(&cfg.db_path())?;
    let port = cfg.dashboard_port;
    let app = scheduler::App::new(cfg, github, store);

    scheduler::start(app.clone()).await;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("dashboard port {port} unavailable (another supervisor running?)"))?;
    println!("[web] dashboard on http://127.0.0.1:{port}");
    axum::serve(listener, web::router(app)).await?;
    Ok(())
}

async fn status(cfg: config::Config) -> Result<()> {
    let url = format!("http://127.0.0.1:{}/api/state", cfg.dashboard_port);
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await;
    let Ok(resp) = resp else {
        println!("supervisor not running (dashboard unreachable)");
        std::process::exit(1);
    };
    let data: serde_json::Value = resp.json().await?;
    for repo in data["repos"].as_array().into_iter().flatten() {
        println!(
            "{}: {}/{} live ({})",
            repo["repo"].as_str().unwrap_or("?"),
            repo["live"],
            repo["pool_size"],
            repo["runtime"].as_str().unwrap_or("?"),
        );
    }
    for r in data["runners"].as_array().into_iter().flatten() {
        let job = r["job"]["job_name"].as_str().unwrap_or("");
        let suffix = if job.is_empty() { String::new() } else { format!("  {job}") };
        println!("  {}  {}{}", r["name"].as_str().unwrap_or("?"), r["state"].as_str().unwrap_or("?"), suffix);
    }
    if let Some(degraded) = data["degraded"].as_object() {
        for (name, reason) in degraded {
            println!("DEGRADED {name}: {}", reason.as_str().unwrap_or("?"));
        }
    }
    Ok(())
}

async fn doctor(cfg: config::Config) -> Result<()> {
    let mut ok = true;

    let mut kinds: Vec<_> = cfg.repos.iter().map(|rc| rc.runtime).collect();
    kinds.dedup();
    for kind in kinds {
        match kind.available().await {
            None => println!("runtime {}: ok", kind.name()),
            Some(reason) => {
                println!("runtime {}: {reason}", kind.name());
                ok = false;
            }
        }
    }

    let github = github::GitHub::new(&cfg.auth_source);
    match github.rate_limit().await {
        Ok(core) => {
            println!("github token: ok ({}/{} requests remaining)", core["remaining"], core["limit"]);
            for rc in &cfg.repos {
                match github.list_runners(&rc.repo).await {
                    Ok(runners) => println!("repo {}: ok ({} registered runner(s))", rc.repo, runners.len()),
                    Err(e) => {
                        println!("repo {}: FAIL {e}", rc.repo);
                        ok = false;
                    }
                }
            }
        }
        Err(e) => {
            println!("github token: FAIL {e}");
            ok = false;
        }
    }

    let mut images: Vec<_> = cfg
        .repos
        .iter()
        .filter(|rc| rc.runtime == config::RuntimeKind::Docker)
        .map(|rc| rc.image.clone())
        .collect();
    images.dedup();
    for image in images {
        let found = StdCommand::new("docker")
            .args(["image", "inspect", &image])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        println!("image {image}: {}", if found { "ok" } else { "MISSING (scripts/build-image.sh)" });
        ok = ok && found;
    }

    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

fn install(config_path: &std::path::Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log_dir = config::expand_tilde("~/Library/Logs/homerunner");
    std::fs::create_dir_all(&log_dir)?;
    let plist_path = config::expand_tilde(&format!("~/Library/LaunchAgents/{PLIST_LABEL}.plist"));

    // launchd's PATH is bare; docker + gh live in /usr/local/bin + /opt/homebrew/bin.
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{PLIST_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>run</string>
    <string>--config</string>
    <string>{config}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin</string></dict>
</dict>
</plist>
"#,
        exe = exe.display(),
        config = config_path.display(),
        log = log_dir.join("homerunner.log").display(),
    );
    std::fs::write(&plist_path, plist)?;

    let uid = StdCommand::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let _ = StdCommand::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{PLIST_LABEL}")])
        .output();
    let status = StdCommand::new("launchctl")
        .args(["bootstrap", &format!("gui/{uid}"), &plist_path.to_string_lossy()])
        .status()?;
    anyhow::ensure!(status.success(), "launchctl bootstrap failed");
    println!("installed + started {PLIST_LABEL}");
    println!("logs: {}", log_dir.join("homerunner.log").display());
    Ok(())
}
