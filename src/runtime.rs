//! Container runtimes. Runners are tagged with labels so any homerunner
//! process can re-discover its containers after a crash. Only concurrency-safe
//! caches are shared as named volumes (uv cache and pnpm store are
//! content-addressed) — never a shared RUNNER_TOOL_CACHE, which races when
//! concurrent setup-* actions populate it.

use crate::config::RuntimeKind;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

pub const MANAGED_LABEL: &str = "homerunner.managed";
pub const REPO_LABEL: &str = "homerunner.repo";
pub const RUNNER_LABEL: &str = "homerunner.runner";

pub const CACHE_VOLUMES: &[(&str, &str)] = &[
    ("homerunner-home-cache", "/home/runner/.cache"),
    ("homerunner-pnpm-store", "/home/runner/.local/share/pnpm/store"),
];

#[derive(Debug, Clone)]
pub struct ManagedContainer {
    pub container_id: String,
    pub runner_name: String,
    pub repo: String,
    pub running: bool,
}

pub struct SpawnSpec<'a> {
    pub runner_name: &'a str,
    pub repo: &'a str,
    pub image: &'a str,
    pub jit_config: &'a str,
    pub registry_mirror: Option<&'a str>,
}

async fn run(argv: &[&str]) -> Result<String> {
    let out = Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .await
        .with_context(|| format!("failed to exec {}", argv[0]))?;
    if !out.status.success() {
        bail!(
            "{} failed: {}",
            argv[..argv.len().min(3)].join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

async fn run_unchecked(argv: &[&str]) {
    let _ = Command::new(argv[0]).args(&argv[1..]).output().await;
}

/// Follow a container's stdout/stderr; lines arrive on the channel, the
/// process is killed when the receiver drops.
fn stream_logs(mut cmd: Command) -> Result<mpsc::Receiver<String>> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child: Child = cmd.spawn().context("failed to spawn log follower")?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>(256);
    let tx2 = tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).await.is_err() {
                break;
            }
        }
        drop(child); // kill_on_drop reaps the follower once both streams end
    });
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx2.send(line).await.is_err() {
                break;
            }
        }
    });
    Ok(rx)
}

impl RuntimeKind {
    pub async fn available(self) -> Option<String> {
        match self {
            RuntimeKind::Docker => match run(&["docker", "info", "--format", "{{.ServerVersion}}"]).await {
                Ok(_) => None,
                Err(e) => Some(format!("docker daemon unreachable: {e}")),
            },
            RuntimeKind::AppleContainer => {
                if std::env::consts::ARCH != "aarch64" {
                    return Some("apple/container requires an Apple Silicon Mac".into());
                }
                match run(&["container", "system", "status"]).await {
                    Ok(_) => None,
                    Err(e) => Some(format!("container system not running (`container system start`): {e}")),
                }
            }
        }
    }

    fn cli(self) -> &'static str {
        match self {
            RuntimeKind::Docker => "docker",
            RuntimeKind::AppleContainer => "container",
        }
    }

    pub async fn spawn(self, spec: &SpawnSpec<'_>) -> Result<String> {
        let managed = format!("{MANAGED_LABEL}=true");
        let repo = format!("{REPO_LABEL}={}", spec.repo);
        let runner = format!("{RUNNER_LABEL}={}", spec.runner_name);
        let jit = format!("HOMERUNNER_JIT_CONFIG={}", spec.jit_config);

        let mut argv: Vec<String> = vec![self.cli().into(), "run".into(), "-d".into()];
        match self {
            RuntimeKind::Docker => argv.push("--privileged".into()),
            // apple/container VMs default to 1GB RAM — far too small for CI.
            RuntimeKind::AppleContainer => {
                argv.extend(["--cpus".into(), "4".into(), "--memory".into(), "6g".into()])
            }
        }
        argv.extend(["--name".into(), spec.runner_name.into()]);
        for label in [&managed, &repo, &runner] {
            argv.extend(["--label".into(), label.clone()]);
        }
        argv.extend(["-e".into(), jit]);
        if let Some(mirror) = spec.registry_mirror {
            argv.extend(["-e".into(), format!("REGISTRY_MIRROR={mirror}")]);
        }
        for (volume, mount) in CACHE_VOLUMES {
            argv.extend(["-v".into(), format!("{volume}:{mount}")]);
        }
        argv.push(spec.image.into());

        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        Ok(run(&refs).await?.trim().to_string())
    }

    pub async fn wait(self, container_id: &str) -> i64 {
        match self {
            RuntimeKind::Docker => match run(&["docker", "wait", container_id]).await {
                Ok(out) => out.trim().parse().unwrap_or(-1),
                Err(_) => -1,
            },
            RuntimeKind::AppleContainer => {
                // `container` has no `wait`; poll inspect until the VM stops.
                loop {
                    match run(&["container", "inspect", container_id]).await {
                        Ok(out) => {
                            let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
                            let info = v.get(0).cloned().unwrap_or(v);
                            if info["status"].as_str() != Some("running") {
                                return info["exitCode"].as_i64().unwrap_or(-1);
                            }
                        }
                        Err(_) => return -1,
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    }

    pub fn logs(self, container_id: &str) -> Result<mpsc::Receiver<String>> {
        let mut cmd = Command::new(self.cli());
        cmd.args(["logs", "-f", container_id]);
        stream_logs(cmd)
    }

    pub async fn list_managed(self) -> Result<Vec<ManagedContainer>> {
        match self {
            RuntimeKind::Docker => {
                let out = run(&[
                    "docker", "ps", "-a",
                    "--filter", &format!("label={MANAGED_LABEL}=true"),
                    "--format", "{{json .}}",
                ])
                .await?;
                let mut containers = Vec::new();
                for line in out.lines() {
                    let row: Value = serde_json::from_str(line)?;
                    let id = row["ID"].as_str().unwrap_or_default().to_string();
                    let inspect_out =
                        run(&["docker", "inspect", "--format", "{{json .}}", &id]).await?;
                    let inspect: Value = serde_json::from_str(inspect_out.trim())?;
                    let labels = &inspect["Config"]["Labels"];
                    containers.push(ManagedContainer {
                        container_id: id,
                        runner_name: labels[RUNNER_LABEL].as_str().unwrap_or_default().into(),
                        repo: labels[REPO_LABEL].as_str().unwrap_or_default().into(),
                        running: inspect["State"]["Running"].as_bool().unwrap_or(false),
                    });
                }
                Ok(containers)
            }
            RuntimeKind::AppleContainer => {
                let out = run(&["container", "ls", "-a", "--format", "json"]).await?;
                let rows: Vec<Value> = serde_json::from_str(&out).unwrap_or_default();
                Ok(rows
                    .iter()
                    .filter(|row| row["labels"][MANAGED_LABEL].as_str() == Some("true"))
                    .map(|row| ManagedContainer {
                        container_id: row["id"].as_str().unwrap_or_default().into(),
                        runner_name: row["labels"][RUNNER_LABEL].as_str().unwrap_or_default().into(),
                        repo: row["labels"][REPO_LABEL].as_str().unwrap_or_default().into(),
                        running: row["status"].as_str() == Some("running"),
                    })
                    .collect())
            }
        }
    }

    /// Copy a path out of a (possibly exited) container to the host.
    pub async fn copy_out(self, container_id: &str, src: &str, dest: &str) -> Result<()> {
        // `container cp` is unverified on apple/container — see docs/arm64-verification.md.
        run(&[self.cli(), "cp", &format!("{container_id}:{src}"), dest]).await?;
        Ok(())
    }

    /// Freeze an exited container's filesystem (workspace included) as an
    /// image for post-mortem `homerunner exec`.
    pub async fn commit_image(self, container_id: &str, tag: &str) -> Result<()> {
        match self {
            RuntimeKind::Docker => {
                run(&[
                    "docker", "commit",
                    "-c", "LABEL homerunner.kept=true",
                    "-c", r#"ENTRYPOINT ["/bin/bash"]"#,
                    container_id, tag,
                ])
                .await?;
                Ok(())
            }
            // No commit equivalent confirmed for apple/container yet.
            RuntimeKind::AppleContainer => bail!("kept workspaces not supported on apple-container yet"),
        }
    }

    pub async fn remove_image(self, tag: &str) {
        run_unchecked(&[self.cli(), "rmi", "-f", tag]).await;
    }

    pub async fn kill(self, container_id: &str) {
        run_unchecked(&[self.cli(), "kill", container_id]).await;
    }

    pub async fn remove(self, container_id: &str) {
        match self {
            // -v: drop the anonymous /var/lib/docker volume from the inner daemon.
            RuntimeKind::Docker => run_unchecked(&["docker", "rm", "-f", "-v", container_id]).await,
            RuntimeKind::AppleContainer => run_unchecked(&["container", "rm", "-f", container_id]).await,
        }
    }
}
