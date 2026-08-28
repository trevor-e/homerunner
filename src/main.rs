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
    /// One-time setup: write config, build the runner image, check access
    Init {
        /// Repo(s) to serve, e.g. --repo you/yourrepo (repeatable)
        #[arg(long)]
        repo: Vec<String>,
        /// Rebuild the runner image even if it already exists
        #[arg(long)]
        rebuild: bool,
    },
    /// (Re)build the runner image from the embedded Dockerfile
    BuildImage,
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

    if let Cmd::Init { repo, rebuild } = &cli.command {
        return init(&path, repo, *rebuild).await;
    }

    let cfg = config::load(&path)?;
    match cli.command {
        Cmd::Run => run(cfg).await,
        Cmd::Status => status(cfg).await,
        Cmd::Doctor => doctor(cfg).await,
        Cmd::BuildImage => build_image(
            &default_image(&cfg),
            cfg.repos.first().map(|rc| rc.runtime).unwrap_or(config::RuntimeKind::Docker),
        ),
        Cmd::Install | Cmd::Init { .. } => unreachable!(),
    }
}

fn default_image(cfg: &config::Config) -> String {
    cfg.repos
        .first()
        .map(|rc| rc.image.clone())
        .unwrap_or_else(|| "homerunner-runner:local".into())
}

fn image_exists(image: &str, runtime: config::RuntimeKind) -> bool {
    let cli = match runtime {
        config::RuntimeKind::Docker => "docker",
        config::RuntimeKind::AppleContainer => "container",
    };
    StdCommand::new(cli)
        .args(["image", "inspect", image])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build the runner image from the Dockerfile + entrypoint embedded in the
/// binary, so a fresh machine needs nothing but this executable and a
/// container runtime. The builder CLI follows the runtime: `docker build`
/// or apple/container's `container build`.
fn build_image(image: &str, runtime: config::RuntimeKind) -> Result<()> {
    let builder = match runtime {
        config::RuntimeKind::Docker => "docker",
        config::RuntimeKind::AppleContainer => "container",
    };
    let dir = std::env::temp_dir().join(format!("homerunner-image-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("Dockerfile"), include_str!("../images/runner/Dockerfile"))?;
    std::fs::write(dir.join("entrypoint.sh"), include_str!("../images/runner/entrypoint.sh"))?;
    println!("building {image} via `{builder} build` (first build downloads the base image; takes a few minutes)");
    let status = StdCommand::new(builder)
        .args(["build", "-t", image, "."])
        .current_dir(&dir)
        .status()
        .with_context(|| format!("failed to run `{builder} build` — is the runtime running?"))?;
    let _ = std::fs::remove_dir_all(&dir);
    anyhow::ensure!(status.success(), "{builder} build failed");
    Ok(())
}

async fn init(path: &std::path::Path, repos: &[String], rebuild: bool) -> Result<()> {
    if path.exists() {
        println!("config: {} (already exists, leaving it alone)", path.display());
    } else {
        anyhow::ensure!(
            !repos.is_empty(),
            "no config at {} — pass at least one --repo owner/name to create it",
            path.display()
        );
        for repo in repos {
            anyhow::ensure!(repo.contains('/'), "--repo must be owner/name, got: {repo}");
        }
        // Arch-aware defaults: Apple Silicon gets the VM-per-job runtime.
        let (runtime, arch) = if std::env::consts::ARCH == "aarch64" {
            ("apple-container", "arm64")
        } else {
            ("docker", "x64")
        };
        let repo_blocks: String = repos
            .iter()
            .map(|repo| format!("\n[[repos]]\nrepo = \"{repo}\"\npool_size = 2\n"))
            .collect();
        let config_body = format!(
            "[defaults]\nruntime = \"{runtime}\"\nlabels = [\"self-hosted\", \"linux\", \"{arch}\"]\n{repo_blocks}"
        );
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, config_body)?;
        println!("config: wrote {}", path.display());
    }

    let cfg = config::load(path)?;
    let image = default_image(&cfg);
    let runtime = cfg.repos.first().map(|rc| rc.runtime).unwrap_or(config::RuntimeKind::Docker);
    if rebuild || !image_exists(&image, runtime) {
        build_image(&image, runtime)?;
    } else {
        println!("image {image}: already built (--rebuild to force)");
    }

    doctor(cfg).await?;
    println!("\nready — `homerunner install` for the launchd agent, or `homerunner run` for foreground");
    Ok(())
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
        println!("image {image}: {}", if found { "ok" } else { "MISSING (homerunner build-image)" });
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
