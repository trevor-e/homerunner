//! Warm-pool scheduler: keeps `reserved` ephemeral JIT runners listening per
//! repo (event-driven — jobs reach runners over the runner's own long-poll,
//! and the supervisor reacts to local container exits and log lines). Repos
//! with `max > reserved` additionally get a slow queued-jobs poll that bursts
//! the pool up to `max`; burst runners decay back to `reserved` when idle.
//! A config where max == reserved everywhere never polls at all.

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
    /// Marked by the watchdog just before killing an idle burst runner, so
    /// the reap path logs it as decay rather than a crash (no backoff).
    pub decaying: bool,
    pub job: Value, // {job_name, job_id, run_id, workflow, html_url, conclusion}
    pub log_tail: VecDeque<String>,
    pub cpu_pct: f64,
    pub mem_bytes: u64,
    pub peak_mem_bytes: u64,
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
    /// Reap outcomes for jobs whose id enrichment hadn't resolved yet,
    /// keyed by runner name; applied (and drained) when enrichment lands.
    pending_outcomes: Mutex<HashMap<String, Value>>,
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
            pending_outcomes: Mutex::new(HashMap::new()),
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
                "reserved": rc.reserved,
                "max": rc.max,
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
                    "cpu_pct": r.cpu_pct,
                    "mem_bytes": r.mem_bytes,
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
    tokio::spawn(stats_sampler(app.clone()));
    tokio::spawn(burst_poller(app.clone()));
    app.log("info", "scheduler", "started");
}

/// Slow queued-jobs poll, only for repos that can burst (max > reserved —
/// which includes fully on-demand repos with reserved = 0). Spawns until
/// every queued job has an idle listener, within per-repo and global caps.
async fn burst_poller(app: Arc<App>) {
    let bursty: Vec<RepoConfig> = app
        .config
        .repos
        .iter()
        .filter(|rc| rc.max > rc.reserved)
        .cloned()
        .collect();
    if bursty.is_empty() {
        return; // pure event-driven config: no polling at all
    }
    loop {
        tokio::time::sleep(Duration::from_secs(app.config.poll_interval_s)).await;
        for repo_cfg in &bursty {
            if app
                .degraded
                .lock()
                .unwrap()
                .contains_key(repo_cfg.runtime.name())
            {
                continue;
            }
            let Ok(queued) = app
                .github
                .count_queued_jobs(&repo_cfg.repo, &repo_cfg.labels)
                .await
            else {
                continue;
            };
            if queued == 0 {
                continue;
            }
            loop {
                let (idle, live_repo) = {
                    let runners = app.runners.lock().unwrap();
                    let idle = runners
                        .values()
                        .filter(|r| {
                            r.state == RunnerState::Listening && r.repo_cfg.repo == repo_cfg.repo
                        })
                        .count() as u32;
                    let live = runners
                        .values()
                        .filter(|r| r.state.live() && r.repo_cfg.repo == repo_cfg.repo)
                        .count() as u32;
                    (idle, live)
                };
                if idle >= queued
                    || live_repo >= repo_cfg.max
                    || app.live_count(None) >= app.config.max_total_runners
                {
                    break;
                }
                app.log(
                    "info",
                    "burst",
                    &format!("{}: {queued} queued job(s), scaling up", repo_cfg.repo),
                );
                spawn_runner(&app, repo_cfg).await;
            }
        }
    }
}

/// Sample per-runner CPU/memory every 15s while runners are live; keep the
/// live values for the dashboard and the per-runner peak for the job journal.
async fn stats_sampler(app: Arc<App>) {
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let by_kind: std::collections::HashMap<crate::config::RuntimeKind, Vec<String>> = {
            let runners = app.runners.lock().unwrap();
            let mut m: std::collections::HashMap<_, Vec<String>> = std::collections::HashMap::new();
            for (name, info) in runners.iter() {
                if info.state.live() {
                    m.entry(info.repo_cfg.runtime)
                        .or_default()
                        .push(name.clone());
                }
            }
            m
        };
        let mut changed = false;
        for (kind, names) in by_kind {
            let samples = kind.stats(&names).await;
            let mut runners = app.runners.lock().unwrap();
            for (name, (cpu, mem)) in samples {
                if let Some(info) = runners.get_mut(&name) {
                    info.cpu_pct = cpu;
                    info.mem_bytes = mem;
                    info.peak_mem_bytes = info.peak_mem_bytes.max(mem);
                    changed = true;
                }
            }
        }
        if changed {
            let _ = app.change_tx.send(());
        }
    }
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
                app.log(
                    "warn",
                    "runtime",
                    &format!("{} degraded: {reason}", kind.name()),
                );
                app.degraded
                    .lock()
                    .unwrap()
                    .insert(kind.name().into(), reason);
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
                            decaying: false,
                            job: json!({}),
                            log_tail: VecDeque::new(),
                            cpu_pct: 0.0,
                            mem_bytes: 0,
                            peak_mem_bytes: 0,
                        },
                    );
                    start_watchers(app, &mc.runner_name, kind, &mc.container_id);
                    app.log(
                        "info",
                        "recover",
                        &format!("re-adopted running runner {}", mc.runner_name),
                    );
                }
                _ => {
                    kind.remove(&mc.container_id).await;
                    app.log(
                        "info",
                        "recover",
                        &format!("reaped stale container {}", mc.runner_name),
                    );
                }
            }
        }
    }
}

/// Delete offline hr-* registrations that have no live container.
async fn sweep_registrations(app: &Arc<App>) {
    for repo_cfg in &app.config.repos {
        let Ok(registrations) = app.github.list_runners(&repo_cfg.repo).await else {
            app.log(
                "warn",
                "recover",
                &format!("registration sweep failed for {}", repo_cfg.repo),
            );
            continue;
        };
        let prefix = format!("hr-{}-", repo_cfg.slug());
        for reg in registrations {
            let name = reg["name"].as_str().unwrap_or_default().to_string();
            let live = app.runners.lock().unwrap().contains_key(&name);
            if name.starts_with(&prefix) && reg["status"].as_str() == Some("offline") && !live {
                if let Some(id) = reg["id"].as_i64() {
                    if app.github.delete_runner(&repo_cfg.repo, id).await.is_ok() {
                        app.log(
                            "info",
                            "recover",
                            &format!("deleted orphan registration {name}"),
                        );
                    }
                }
            }
        }
    }
}

// -- pool management ---------------------------------------------------------

async fn top_up(app: &Arc<App>, repo_cfg: &RepoConfig) {
    if app
        .degraded
        .lock()
        .unwrap()
        .contains_key(repo_cfg.runtime.name())
    {
        return;
    }
    while app.live_count(Some(&repo_cfg.repo)) < repo_cfg.reserved
        && app.live_count(None) < app.config.max_total_runners
    {
        spawn_runner(app, repo_cfg).await;
    }
}

async fn spawn_runner(app: &Arc<App>, repo_cfg: &RepoConfig) {
    let name = format!(
        "hr-{}-{:06x}",
        repo_cfg.slug(),
        rand::random::<u32>() & 0xff_ffff
    );
    let mut info = RunnerInfo {
        repo_cfg: repo_cfg.clone(),
        state: RunnerState::Listening,
        container_id: String::new(),
        gh_runner_id: None,
        created_at: now(),
        busy_at: None,
        ran_job: false,
        decaying: false,
        job: json!({}),
        log_tail: VecDeque::new(),
        cpu_pct: 0.0,
        mem_bytes: 0,
        peak_mem_bytes: 0,
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
            app.log(
                "info",
                "spawn",
                &format!("{name} listening for {}", repo_cfg.repo),
            );
        }
        Err(err) => {
            info.state = RunnerState::Failed;
            app.runners.lock().unwrap().insert(name.clone(), info);
            *app.backoff
                .lock()
                .unwrap()
                .entry(repo_cfg.repo.clone())
                .or_default() += 1;
            app.record_runner(&name, Some(now()), None);
            app.log("error", "spawn", &format!("{name} failed: {err}"));
        }
    }
}

fn start_watchers(
    app: &Arc<App>,
    name: &str,
    kind: crate::config::RuntimeKind,
    container_id: &str,
) {
    tokio::spawn(watch_exit(
        app.clone(),
        name.to_string(),
        kind,
        container_id.to_string(),
    ));
    tokio::spawn(watch_logs(
        app.clone(),
        name.to_string(),
        kind,
        container_id.to_string(),
    ));
}

// -- per-runner watchers (local events only) ---------------------------------

async fn watch_exit(
    app: Arc<App>,
    name: String,
    kind: crate::config::RuntimeKind,
    container_id: String,
) {
    let code = kind.wait(&container_id).await;

    // The conclusion arrives via the log watcher; give it a moment to land
    // before deciding whether this workspace is worth keeping.
    for _ in 0..6 {
        let settled = app
            .runners
            .lock()
            .unwrap()
            .get(&name)
            .map(|i| !i.ran_job || !i.job["conclusion"].is_null())
            .unwrap_or(true);
        if settled {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let Some(info) = app.runners.lock().unwrap().get(&name).cloned() else {
        kind.remove(&container_id).await;
        return;
    };
    let (repo_cfg, ran_job) = (info.repo_cfg.clone(), info.ran_job);
    let job_id = info.job["job_id"].as_i64();

    // Post-mortem capture, before the container is destroyed: the runner's
    // _diag logs (verbose per-step output) always; the whole workspace as a
    // kept image when the job failed.
    let mut log_dir: Option<String> = None;
    let mut kept_image: Option<String> = None;
    if ran_job {
        let key = job_id
            .map(|i| i.to_string())
            .unwrap_or_else(|| name.clone());
        let dir = app.config.data_dir.join("jobs").join(&key);
        let _ = std::fs::create_dir_all(&dir);
        let diag_dest = dir.join("diag");
        if kind
            .copy_out(
                &container_id,
                "/home/runner/_diag",
                &diag_dest.to_string_lossy(),
            )
            .await
            .is_ok()
        {
            log_dir = Some(dir.to_string_lossy().into_owned());
        }
        let meta = json!({
            "job": info.job, "runner": name, "repo": repo_cfg.repo, "exit_code": code,
        });
        let _ = std::fs::write(dir.join("meta.json"), meta.to_string());

        if info.job["conclusion"].as_str() == Some("failure")
            && app.config.keep_failed_workspaces > 0
        {
            let tag = format!("homerunner-kept:{key}");
            match kind.commit_image(&container_id, &tag).await {
                Ok(()) => {
                    kept_image = Some(tag.clone());
                    app.log(
                        "info",
                        "keep",
                        &format!("kept failed workspace as {tag} (homerunner exec {key})"),
                    );
                }
                Err(e) => app.log(
                    "warn",
                    "keep",
                    &format!("could not keep workspace for {name}: {e}"),
                ),
            }
        }
    }
    let oom = ran_job && kind.oom_killed(&container_id).await;
    kind.remove(&container_id).await;
    let peak_mb = (info.peak_mem_bytes as f64 / (1024.0 * 1024.0)).round();
    if let Some(id) = job_id {
        let store = app.store.lock().unwrap();
        store.set_job_artifacts(id, log_dir.as_deref(), kept_image.as_deref());
        // The log-line handler can only persist the conclusion if enrichment
        // had already resolved the job id by then; settle it here regardless.
        if let Some(conclusion) = info.job["conclusion"].as_str() {
            store.job_concluded(id, conclusion);
        }
        if peak_mb > 0.0 || oom {
            store.set_job_resources(id, peak_mb, oom);
        }
    } else if ran_job {
        // Enrichment hasn't found the job id yet (short jobs often outrun the
        // API); park the outcome for enrich_job to apply when it lands.
        app.pending_outcomes.lock().unwrap().insert(
            name.clone(),
            json!({
                "conclusion": info.job["conclusion"],
                "log_dir": log_dir,
                "kept_image": kept_image,
                "peak_mem_mb": peak_mb,
                "oom": oom,
            }),
        );
    }
    if oom {
        app.log("warn", "reap", &format!("{name} was OOM-killed"));
    }
    gc_kept_images(&app, kind).await;
    prune_job_logs(&app);

    {
        let mut runners = app.runners.lock().unwrap();
        if let Some(info) = runners.get_mut(&name) {
            info.state = RunnerState::Exited;
        }
    }
    app.record_runner(&name, Some(now()), Some(code));
    app.runners.lock().unwrap().remove(&name);

    let delay = {
        let mut backoff = app.backoff.lock().unwrap();
        let entry = backoff.entry(repo_cfg.repo.clone()).or_default();
        if ran_job {
            *entry = 0;
            0
        } else if info.decaying {
            0 // deliberate scale-down, not a crash
        } else {
            *entry += 1;
            60u64.min(2u64.pow((*entry).min(6)))
        }
    };
    if ran_job {
        app.log(
            "info",
            "reap",
            &format!("{name} finished its job (exit {code})"),
        );
    } else {
        if !info.decaying {
            app.log(
                "warn",
                "reap",
                &format!("{name} exited without a job (exit {code})"),
            );
        }
        // A runner that never took a job leaves its JIT registration behind.
        if let Some(gh_id) = info.gh_runner_id {
            let _ = app.github.delete_runner(&repo_cfg.repo, gh_id).await;
        }
    }
    app.update_caffeinate();

    if delay > 0 {
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
    top_up(&app, &repo_cfg).await;
}

async fn watch_logs(
    app: Arc<App>,
    name: String,
    kind: crate::config::RuntimeKind,
    container_id: String,
) {
    let Ok(mut lines) = kind.logs(&container_id) else {
        return;
    };
    while let Some(line) = lines.recv().await {
        let mut became_busy = false;
        let mut concluded: Option<(i64, String)> = None;
        {
            let mut runners = app.runners.lock().unwrap();
            let Some(info) = runners.get_mut(&name) else {
                break;
            };
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
            app.log(
                "info",
                "job",
                &format!(
                    "{name} running: {}",
                    app.runners.lock().unwrap()[&name].job["job_name"]
                        .as_str()
                        .unwrap_or("?")
                ),
            );
            tokio::spawn(enrich_job(app.clone(), name.clone()));
        }
        if let Some((job_id, conclusion)) = concluded {
            app.store.lock().unwrap().job_concluded(job_id, &conclusion);
            let _ = app.change_tx.send(());
        }
    }
}

/// A few REST lookups per busy transition, keyed to the runner name. Retries
/// past runner exit: a short job can be reaped before the jobs API ever
/// reported its runner_name, and its reap outcome waits in pending_outcomes.
async fn enrich_job(app: Arc<App>, name: String) {
    let (repo, busy_at) = {
        let runners = app.runners.lock().unwrap();
        let Some(info) = runners.get(&name) else {
            return;
        };
        (info.repo_cfg.repo.clone(), info.busy_at)
    };
    for attempt in 0..8 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(8)).await;
        }
        match app.github.find_job_by_runner(&repo, &name).await {
            Err(_) => return,
            Ok(None) => continue,
            Ok(Some(found)) => {
                {
                    let mut runners = app.runners.lock().unwrap();
                    if let Some(info) = runners.get_mut(&name) {
                        for key in [
                            "job_id",
                            "run_id",
                            "workflow",
                            "html_url",
                            "head_branch",
                            "head_sha",
                            "title",
                            "event",
                        ] {
                            info.job[key] = found[key].clone();
                        }
                        if info.job["job_name"].is_null() {
                            info.job["job_name"] = found["job_name"].clone();
                        }
                    }
                }
                let job_id = found["job_id"].as_i64();
                let pending = app.pending_outcomes.lock().unwrap().remove(&name);
                {
                    let store = app.store.lock().unwrap();
                    store.job_started(&found, &repo, &name, busy_at);
                    // The runner may already be reaped; apply its parked outcome.
                    if let (Some(id), Some(outcome)) = (job_id, pending.as_ref()) {
                        if let Some(conclusion) = outcome["conclusion"].as_str() {
                            store.job_concluded(id, conclusion);
                        }
                        store.set_job_artifacts(
                            id,
                            outcome["log_dir"].as_str(),
                            outcome["kept_image"].as_str(),
                        );
                        if outcome["peak_mem_mb"].as_f64().unwrap_or(0.0) > 0.0
                            || outcome["oom"].as_bool() == Some(true)
                        {
                            store.set_job_resources(
                                id,
                                outcome["peak_mem_mb"].as_f64().unwrap_or(0.0),
                                outcome["oom"].as_bool() == Some(true),
                            );
                        }
                    }
                }
                let _ = app.change_tx.send(());
                return;
            }
        }
    }
}

/// Keep only the newest `keep_job_logs` captured-log dirs; older ones are
/// removed and their journal references cleared.
fn prune_job_logs(app: &Arc<App>) {
    let jobs_dir = app.config.data_dir.join("jobs");
    let Ok(entries) = std::fs::read_dir(&jobs_dir) else {
        return;
    };
    let mut dirs: Vec<(std::path::PathBuf, std::time::SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let modified = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (e.path(), modified)
        })
        .collect();
    let budget = app.config.keep_job_logs as usize;
    if dirs.len() <= budget {
        return;
    }
    dirs.sort_by_key(|(_, modified)| *modified);
    for (path, _) in dirs.iter().take(dirs.len() - budget) {
        let _ = std::fs::remove_dir_all(path);
        app.store
            .lock()
            .unwrap()
            .clear_log_dir(&path.to_string_lossy());
        app.log(
            "info",
            "prune",
            &format!("removed old job logs {}", path.display()),
        );
    }
}

/// Drop the oldest kept post-mortem images beyond the configured budget.
async fn gc_kept_images(app: &Arc<App>, kind: crate::config::RuntimeKind) {
    let excess: Vec<(i64, String)> = {
        let store = app.store.lock().unwrap();
        let all = store.kept_images();
        let budget = app.config.keep_failed_workspaces as usize;
        if all.len() > budget {
            all[..all.len() - budget].to_vec()
        } else {
            Vec::new()
        }
    };
    for (job_id, tag) in excess {
        kind.remove_image(&tag).await;
        app.store.lock().unwrap().clear_kept_image(job_id);
        app.log("info", "keep", &format!("gc'd kept workspace {tag}"));
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
            app.log(
                "error",
                "watchdog",
                &format!("{name} exceeded job timeout; killing"),
            );
            kind.kill(&container_id).await;
        }
        // Idle burst runners beyond the reserved floor decay after a while.
        let decay_after = app.config.idle_decay_min as f64 * 60.0;
        let mut to_decay: Vec<(String, crate::config::RuntimeKind, String)> = Vec::new();
        {
            let mut runners = app.runners.lock().unwrap();
            for rc in &app.config.repos {
                let live = runners
                    .values()
                    .filter(|r| r.state.live() && r.repo_cfg.repo == rc.repo)
                    .count() as u32;
                if live <= rc.reserved {
                    continue;
                }
                let mut idle_old: Vec<(String, f64)> = runners
                    .iter()
                    .filter(|(_, r)| {
                        r.state == RunnerState::Listening
                            && !r.decaying
                            && r.repo_cfg.repo == rc.repo
                            && ts - r.created_at > decay_after
                    })
                    .map(|(n, r)| (n.clone(), r.created_at))
                    .collect();
                idle_old.sort_by(|a, b| a.1.total_cmp(&b.1));
                for (name, _) in idle_old.into_iter().take((live - rc.reserved) as usize) {
                    if let Some(info) = runners.get_mut(&name) {
                        info.decaying = true;
                        to_decay.push((name, info.repo_cfg.runtime, info.container_id.clone()));
                    }
                }
            }
        }
        for (name, kind, container_id) in to_decay {
            app.log(
                "info",
                "decay",
                &format!("{name} idle beyond reserved; scaling down"),
            );
            kind.kill(&container_id).await;
        }
        if ts - last_release_check > 86_400.0 {
            last_release_check = ts;
            app.store.lock().unwrap().prune_events(7.0 * 86_400.0);
            if let Ok(latest) = app.github.latest_runner_release().await {
                app.log(
                    "info",
                    "staleness",
                    &format!("latest actions/runner release: v{latest}"),
                );
            }
        }
    }
}
