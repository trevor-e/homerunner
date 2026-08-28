//! Warm-pool scheduler: keeps `pool_size` ephemeral JIT runners listening per
//! repo. Event-driven only — jobs reach runners over the runner's own
//! long-poll to GitHub; the supervisor reacts to local container exits and
//! log lines, never to REST polling.

use crate::config::{Config, RepoConfig};
use crate::github::GitHub;
use crate::store::Store;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

static RUNNING_JOB_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Running job: (?<job>.+)$").unwrap());
static COMPLETED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"completed with result: (?<result>\w+)").unwrap());

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerState {
    Listening,
    Busy,
    Exited,
    Failed,
}

impl RunnerState {
    fn name(self) -> &'static str {
        match self {
            RunnerState::Listening => "listening",
            RunnerState::Busy => "busy",
            RunnerState::Exited => "exited",
            RunnerState::Failed => "failed",
        }
    }
    fn live(self) -> bool {
        matches!(self, RunnerState::Listening | RunnerState::Busy)
    }
}

#[derive(Debug, Clone)]
pub struct RunnerInfo {
    pub repo_cfg: RepoConfig,
    pub state: RunnerState,
    pub container_id: String,
    pub gh_runner_id: Option<i64>,
    pub created_at: f64,
    pub busy_at: Option<f64>,
    pub ran_job: bool,
    pub job: Value, // {job_name, job_id, run_id, workflow, html_url, conclusion}
    pub log_tail: VecDeque<String>,
}

pub struct App {
    pub config: Config,
    pub github: GitHub,
    pub store: Mutex<Store>,
    pub runners: Mutex<HashMap<String, RunnerInfo>>,
    pub degraded: Mutex<HashMap<String, String>>,
    backoff: Mutex<HashMap<String, u32>>,
    pub change_tx: broadcast::Sender<()>,
    caffeinate: Mutex<Option<tokio::process::Child>>,
}

impl App {
    pub fn new(config: Config, github: GitHub, store: Store) -> Arc<Self> {
        Arc::new(Self {
            config,
            github,
            store: Mutex::new(store),
            runners: Mutex::new(HashMap::new()),
            degraded: Mutex::new(HashMap::new()),
            backoff: Mutex::new(HashMap::new()),
            change_tx: broadcast::channel(64).0,
            caffeinate: Mutex::new(None),
        })
    }

    pub fn log(&self, level: &str, source: &str, msg: &str) {
        println!("[{source}] {msg}");
        self.store.lock().unwrap().event(level, source, msg);
        let _ = self.change_tx.send(());
    }

    fn live_count(&self, repo: Option<&str>) -> u32 {
        self.runners
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.state.live() && repo.is_none_or(|repo| r.repo_cfg.repo == repo))
            .count() as u32
    }

    fn record_runner(&self, name: &str, ended_at: Option<f64>, exit_code: Option<i64>) {
        let Some(info) = self.runners.lock().unwrap().get(name).cloned() else {
            return;
        };
        self.store.lock().unwrap().record_runner(
            name,
            &info.repo_cfg.repo,
            info.repo_cfg.runtime.name(),
            &info.container_id,
            info.gh_runner_id,
            info.state.name(),
            info.created_at,
            ended_at,
            exit_code,
        );
    }

    fn update_caffeinate(&self) {
        let wanted = self
            .runners
            .lock()
            .unwrap()
            .values()
            .any(|r| r.state == RunnerState::Busy && r.repo_cfg.caffeinate);
        let mut guard = self.caffeinate.lock().unwrap();
        if wanted && guard.is_none() {
            *guard = tokio::process::Command::new("caffeinate")
                .arg("-is")
                .kill_on_drop(true)
                .spawn()
                .ok(); // absent on non-macOS: fine, jobs just don't hold wake
        } else if !wanted {
            if let Some(mut child) = guard.take() {
                let _ = child.start_kill();
            }
        }
    }

    pub fn snapshot(&self) -> Value {
        let runners = self.runners.lock().unwrap();
        json!({
            "degraded": *self.degraded.lock().unwrap(),
            "repos": self.config.repos.iter().map(|rc| json!({
                "repo": rc.repo,
                "runtime": rc.runtime.name(),
                "pool_size": rc.pool_size,
                "live": runners.values()
                    .filter(|r| r.state.live() && r.repo_cfg.repo == rc.repo).count(),
            })).collect::<Vec<_>>(),
            "runners": runners.iter()
                .filter(|(_, r)| r.state.live())
                .map(|(name, r)| json!({
                    "name": name,
                    "repo": r.repo_cfg.repo,
                    "state": r.state.name(),
                    "created_at": r.created_at,
                    "busy_at": r.busy_at,
                    "job": r.job,
                    "log_tail": r.log_tail.iter().rev().take(8).rev().collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
        })
    }
}

// -- lifecycle ---------------------------------------------------------------

pub async fn start(app: Arc<App>) {
    check_runtimes(&app).await;
    adopt_orphans(&app).await;
    sweep_registrations(&app).await;
    for repo_cfg in app.config.repos.clone() {
        top_up(&app, &repo_cfg).await;
    }
    tokio::spawn(watchdog(app.clone()));
    app.log("info", "scheduler", "started");
}

async fn check_runtimes(app: &Arc<App>) {
    let kinds: Vec<_> = {
        let mut kinds: Vec<_> = app.config.repos.iter().map(|rc| rc.runtime).collect();
        kinds.dedup();
        kinds
    };
    for kind in kinds {
        match kind.available().await {
            Some(reason) => {
                app.log("warn", "runtime", &format!("{} degraded: {reason}", kind.name()));
                app.degraded.lock().unwrap().insert(kind.name().into(), reason);
            }
            None => {
                app.degraded.lock().unwrap().remove(kind.name());
            }
        }
    }
}

/// Crash recovery: re-adopt live managed containers, reap exited ones.
async fn adopt_orphans(app: &Arc<App>) {
    let degraded: Vec<String> = app.degraded.lock().unwrap().keys().cloned().collect();
    let mut kinds: Vec<_> = app.config.repos.iter().map(|rc| rc.runtime).collect();
    kinds.dedup();
    for kind in kinds {
        if degraded.contains(&kind.name().to_string()) {
            continue;
        }
        let Ok(containers) = kind.list_managed().await else {
            continue;
        };
        for mc in containers {
            let repo_cfg = app
                .config
                .repos
                .iter()
                .find(|rc| rc.repo == mc.repo && rc.runtime == kind)
                .cloned();
            match (mc.running, repo_cfg) {
                (true, Some(repo_cfg)) => {
                    app.runners.lock().unwrap().insert(
                        mc.runner_name.clone(),
                        RunnerInfo {
                            repo_cfg,
                            state: RunnerState::Listening,
                            container_id: mc.container_id.clone(),
                            gh_runner_id: None,
                            created_at: now(),
                            busy_at: None,
                            ran_job: false,
                            job: json!({}),
                            log_tail: VecDeque::new(),
                        },
                    );
                    start_watchers(app, &mc.runner_name, kind, &mc.container_id);
                    app.log("info", "recover", &format!("re-adopted running runner {}", mc.runner_name));
                }
                _ => {
                    kind.remove(&mc.container_id).await;
                    app.log("info", "recover", &format!("reaped stale container {}", mc.runner_name));
                }
            }
        }
    }
}

/// Delete offline hr-* registrations that have no live container.
async fn sweep_registrations(app: &Arc<App>) {
    for repo_cfg in &app.config.repos {
        let Ok(registrations) = app.github.list_runners(&repo_cfg.repo).await else {
            app.log("warn", "recover", &format!("registration sweep failed for {}", repo_cfg.repo));
            continue;
        };
        let prefix = format!("hr-{}-", repo_cfg.slug());
        for reg in registrations {
            let name = reg["name"].as_str().unwrap_or_default().to_string();
            let live = app.runners.lock().unwrap().contains_key(&name);
            if name.starts_with(&prefix) && reg["status"].as_str() == Some("offline") && !live {
                if let Some(id) = reg["id"].as_i64() {
                    if app.github.delete_runner(&repo_cfg.repo, id).await.is_ok() {
                        app.log("info", "recover", &format!("deleted orphan registration {name}"));
                    }
                }
            }
        }
    }
}

// -- pool management ---------------------------------------------------------

async fn top_up(app: &Arc<App>, repo_cfg: &RepoConfig) {
    if app.degraded.lock().unwrap().contains_key(repo_cfg.runtime.name()) {
        return;
    }
    while app.live_count(Some(&repo_cfg.repo)) < repo_cfg.pool_size
        && app.live_count(None) < app.config.max_total_runners
    {
        spawn_runner(app, repo_cfg).await;
    }
}

async fn spawn_runner(app: &Arc<App>, repo_cfg: &RepoConfig) {
    let name = format!("hr-{}-{:06x}", repo_cfg.slug(), rand::random::<u32>() & 0xff_ffff);
    let mut info = RunnerInfo {
        repo_cfg: repo_cfg.clone(),
        state: RunnerState::Listening,
        container_id: String::new(),
        gh_runner_id: None,
        created_at: now(),
        busy_at: None,
        ran_job: false,
        job: json!({}),
        log_tail: VecDeque::new(),
    };

    let spawned = async {
        let (gh_id, jit) = app
            .github
            .generate_jitconfig(&repo_cfg.repo, &name, &repo_cfg.labels)
            .await?;
        let container_id = repo_cfg
            .runtime
            .spawn(&crate::runtime::SpawnSpec {
                runner_name: &name,
                repo: &repo_cfg.repo,
                image: &repo_cfg.image,
                jit_config: &jit,
                registry_mirror: repo_cfg.registry_mirror.as_deref(),
            })
            .await?;
        anyhow::Ok((gh_id, container_id))
    }
    .await;

    match spawned {
        Ok((gh_id, container_id)) => {
            info.gh_runner_id = Some(gh_id);
            info.container_id = container_id.clone();
            app.runners.lock().unwrap().insert(name.clone(), info);
            start_watchers(app, &name, repo_cfg.runtime, &container_id);
            app.record_runner(&name, None, None);
            app.log("info", "spawn", &format!("{name} listening for {}", repo_cfg.repo));
        }
        Err(err) => {
            info.state = RunnerState::Failed;
            app.runners.lock().unwrap().insert(name.clone(), info);
            *app.backoff.lock().unwrap().entry(repo_cfg.repo.clone()).or_default() += 1;
            app.record_runner(&name, Some(now()), None);
            app.log("error", "spawn", &format!("{name} failed: {err}"));
        }
    }
}

fn start_watchers(app: &Arc<App>, name: &str, kind: crate::config::RuntimeKind, container_id: &str) {
    tokio::spawn(watch_exit(app.clone(), name.to_string(), kind, container_id.to_string()));
    tokio::spawn(watch_logs(app.clone(), name.to_string(), kind, container_id.to_string()));
}

// -- per-runner watchers (local events only) ---------------------------------

async fn watch_exit(app: Arc<App>, name: String, kind: crate::config::RuntimeKind, container_id: String) {
    let code = kind.wait(&container_id).await;
    kind.remove(&container_id).await;

    let (repo_cfg, ran_job) = {
        let mut runners = app.runners.lock().unwrap();
        let Some(info) = runners.get_mut(&name) else { return };
        info.state = RunnerState::Exited;
        (info.repo_cfg.clone(), info.ran_job)
    };
    app.record_runner(&name, Some(now()), Some(code));
    app.runners.lock().unwrap().remove(&name);

    let delay = {
        let mut backoff = app.backoff.lock().unwrap();
        let entry = backoff.entry(repo_cfg.repo.clone()).or_default();
        if ran_job {
            *entry = 0;
            0
        } else {
            *entry += 1;
            60u64.min(2u64.pow((*entry).min(6)))
        }
    };
    if ran_job {
        app.log("info", "reap", &format!("{name} finished its job (exit {code})"));
    } else {
        app.log("warn", "reap", &format!("{name} exited without a job (exit {code})"));
    }
    app.update_caffeinate();

    if delay > 0 {
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
    top_up(&app, &repo_cfg).await;
}

async fn watch_logs(app: Arc<App>, name: String, kind: crate::config::RuntimeKind, container_id: String) {
    let Ok(mut lines) = kind.logs(&container_id) else { return };
    while let Some(line) = lines.recv().await {
        let mut became_busy = false;
        let mut concluded: Option<(i64, String)> = None;
        {
            let mut runners = app.runners.lock().unwrap();
            let Some(info) = runners.get_mut(&name) else { break };
            info.log_tail.push_back(line.clone());
            if info.log_tail.len() > 40 {
                info.log_tail.pop_front();
            }
            if let Some(caps) = RUNNING_JOB_RE.captures(&line) {
                // The runner prints each diagnostic both bare and timestamped.
                if info.state != RunnerState::Busy {
                    info.state = RunnerState::Busy;
                    info.busy_at = Some(now());
                    info.ran_job = true;
                    info.job = json!({"job_name": &caps["job"]});
                    became_busy = true;
                }
            } else if let Some(caps) = COMPLETED_RE.captures(&line) {
                if info.job["conclusion"].is_null() {
                    let conclusion = match &caps["result"] {
                        "Succeeded" => "success".to_string(),
                        "Failed" => "failure".to_string(),
                        "Canceled" => "cancelled".to_string(),
                        other => other.to_lowercase(),
                    };
                    info.job["conclusion"] = json!(conclusion);
                    if let Some(job_id) = info.job["job_id"].as_i64() {
                        concluded = Some((job_id, conclusion.clone()));
                    }
                    app.log("info", "job", &format!("{name} job result: {conclusion}"));
                }
            }
        }
        if became_busy {
            app.update_caffeinate();
            app.log("info", "job", &format!("{name} running: {}", app.runners.lock().unwrap()[&name].job["job_name"].as_str().unwrap_or("?")));
            tokio::spawn(enrich_job(app.clone(), name.clone()));
        }
        if let Some((job_id, conclusion)) = concluded {
            app.store.lock().unwrap().job_concluded(job_id, &conclusion);
            let _ = app.change_tx.send(());
        }
    }
}

/// A few REST lookups per busy transition, purely for the dashboard. Retries
/// because the jobs API lags the runner's own log line in reporting
/// runner_name.
async fn enrich_job(app: Arc<App>, name: String) {
    for _ in 0..5 {
        let (repo, busy_at, still_busy) = {
            let runners = app.runners.lock().unwrap();
            let Some(info) = runners.get(&name) else { return };
            (info.repo_cfg.repo.clone(), info.busy_at, info.state == RunnerState::Busy)
        };
        if !still_busy {
            return;
        }
        match app.github.find_job_by_runner(&repo, &name).await {
            Err(_) => return,
            Ok(Some(found)) => {
                {
                    let mut runners = app.runners.lock().unwrap();
                    if let Some(info) = runners.get_mut(&name) {
                        for key in ["job_id", "run_id", "workflow", "html_url"] {
                            info.job[key] = found[key].clone();
                        }
                        if info.job["job_name"].is_null() {
                            info.job["job_name"] = found["job_name"].clone();
                        }
                    }
                }
                app.store.lock().unwrap().job_started(&found, &repo, &name, busy_at);
                let _ = app.change_tx.send(());
                return;
            }
            Ok(None) => tokio::time::sleep(Duration::from_secs(8)).await,
        }
    }
}

/// Local timers: wedged-job kill + daily runner-release staleness note.
async fn watchdog(app: Arc<App>) {
    let mut last_release_check = 0f64;
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let ts = now();
        let stuck: Vec<(String, crate::config::RuntimeKind, String)> = app
            .runners
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, r)| {
                r.state == RunnerState::Busy
                    && r.busy_at
                        .is_some_and(|b| ts - b > (r.repo_cfg.job_timeout_min * 60) as f64)
            })
            .map(|(n, r)| (n.clone(), r.repo_cfg.runtime, r.container_id.clone()))
            .collect();
        for (name, kind, container_id) in stuck {
            app.log("error", "watchdog", &format!("{name} exceeded job timeout; killing"));
            kind.kill(&container_id).await;
        }
        if ts - last_release_check > 86_400.0 {
            last_release_check = ts;
            if let Ok(latest) = app.github.latest_runner_release().await {
                app.log("info", "staleness", &format!("latest actions/runner release: v{latest}"));
            }
        }
    }
}
