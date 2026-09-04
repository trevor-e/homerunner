//! Config loading: TOML -> structs, with [defaults] merged into each repo.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeKind {
    Docker,
    AppleContainer,
}

impl RuntimeKind {
    pub fn name(self) -> &'static str {
        match self {
            RuntimeKind::Docker => "docker",
            RuntimeKind::AppleContainer => "apple-container",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RepoDefaults {
    runtime: Option<RuntimeKind>,
    labels: Option<Vec<String>>,
    image: Option<String>,
    reserved: Option<u32>,
    max: Option<u32>,
    job_timeout_min: Option<u64>,
    caffeinate: Option<bool>,
    registry_mirror: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RepoEntry {
    repo: String,
    #[serde(flatten)]
    overrides: RepoDefaults,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct SupervisorSection {
    dashboard_port: Option<u16>,
    max_total_runners: Option<u32>,
    data_dir: Option<String>,
    keep_failed_workspaces: Option<u32>,
    failed_workspaces_max_age_days: Option<u64>,
    failed_workspaces_max_mb: Option<u64>,
    keep_job_logs: Option<u32>,
    job_logs_max_age_days: Option<u64>,
    job_logs_max_mb: Option<u64>,
    job_history_days: Option<u64>,
    event_history_days: Option<u64>,
    service_log_max_mb: Option<u64>,
    service_log_backups: Option<u32>,
    poll_interval_s: Option<u64>,
    idle_decay_min: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct AuthSection {
    source: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ToolchainsSection {
    python: Option<String>,
    node: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    supervisor: SupervisorSection,
    #[serde(default)]
    auth: AuthSection,
    #[serde(default)]
    toolchains: ToolchainsSection,
    #[serde(default)]
    defaults: RepoDefaults,
    #[serde(default)]
    repos: Vec<RepoEntry>,
    #[serde(default)]
    monitors: Vec<MonitorConfig>,
}

/// A local CI guardrail. Scope fields are exact matches; `log_pattern` is a
/// regular expression evaluated against the captured worker log.
#[derive(Debug, Clone, Deserialize)]
pub struct MonitorConfig {
    pub name: String,
    pub repo: Option<String>,
    pub workflow: Option<String>,
    pub job_name: Option<String>,
    pub branch: Option<String>,
    pub duration_min: Option<u64>,
    pub consecutive_failures: Option<u32>,
    #[serde(default)]
    pub oom: bool,
    pub log_pattern: Option<String>,
    #[serde(default)]
    pub retain_workspace: bool,
}

#[derive(Debug, Clone)]
pub struct RepoConfig {
    pub repo: String, // "owner/name"
    pub runtime: RuntimeKind,
    pub labels: Vec<String>,
    pub image: String,
    /// Warm listeners always kept alive (0 = fully on-demand).
    pub reserved: u32,
    /// Burst ceiling: the poller may scale up to this many concurrent runners.
    pub max: u32,
    pub job_timeout_min: u64,
    pub caffeinate: bool,
    pub registry_mirror: Option<String>,
}

impl RepoConfig {
    pub fn slug(&self) -> String {
        self.repo.replace('/', "-")
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub dashboard_port: u16,
    pub max_total_runners: u32,
    pub data_dir: PathBuf,
    /// How many failed-job workspaces to keep as post-mortem images (0 = off).
    pub keep_failed_workspaces: u32,
    /// Optional maximum age and aggregate size for post-mortem images.
    pub failed_workspaces_max_age_days: Option<u64>,
    pub failed_workspaces_max_bytes: Option<u64>,
    /// How many jobs' captured log dirs to keep (oldest pruned at reap).
    pub keep_job_logs: u32,
    /// Optional maximum age and aggregate size for captured job directories.
    pub job_logs_max_age_days: Option<u64>,
    pub job_logs_max_bytes: Option<u64>,
    /// Completed job metadata older than this is pruned once artifacts are gone.
    pub job_history_days: u64,
    pub event_history_days: u64,
    /// Rotation policy used by the installed launchd service log.
    pub service_log_max_bytes: u64,
    pub service_log_backups: u32,
    /// Queued-jobs poll cadence for repos that can burst (max > reserved).
    pub poll_interval_s: u64,
    /// Minutes an idle burst runner (beyond reserved) lives before decay.
    pub idle_decay_min: u64,
    pub auth_source: String,
    pub python_version: String,
    pub node_version: String,
    pub repos: Vec<RepoConfig>,
    pub monitors: Vec<MonitorConfig>,
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("homerunner.db")
    }
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

pub fn load(path: &Path) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("config file not found: {}", path.display()))?;
    let raw: RawConfig = toml::from_str(&raw).context("invalid config TOML")?;
    let d = &raw.defaults;

    let mut repos = Vec::new();
    for entry in &raw.repos {
        if !entry.repo.contains('/') {
            bail!(
                "[[repos]] entry needs repo = \"owner/name\", got: {}",
                entry.repo
            );
        }
        let o = &entry.overrides;
        repos.push(RepoConfig {
            repo: entry.repo.clone(),
            runtime: o.runtime.or(d.runtime).unwrap_or(RuntimeKind::Docker),
            labels: o
                .labels
                .clone()
                .or_else(|| d.labels.clone())
                .unwrap_or_else(|| vec!["self-hosted".into(), "linux".into(), "x64".into()]),
            image: o
                .image
                .clone()
                .or_else(|| d.image.clone())
                .unwrap_or_else(|| "homerunner-runner:local".into()),
            reserved: o.reserved.or(d.reserved).unwrap_or(1),
            max: o.max.or(d.max).unwrap_or(2),
            job_timeout_min: o.job_timeout_min.or(d.job_timeout_min).unwrap_or(120),
            caffeinate: o
                .caffeinate
                .or(d.caffeinate)
                .unwrap_or(cfg!(target_os = "macos")),
            registry_mirror: o
                .registry_mirror
                .clone()
                .or_else(|| d.registry_mirror.clone()),
        });
    }
    if repos.is_empty() {
        bail!("no [[repos]] configured");
    }
    for rc in &repos {
        if rc.max < rc.reserved {
            bail!(
                "repo {}: max ({}) must be >= reserved ({})",
                rc.repo,
                rc.max,
                rc.reserved
            );
        }
        if rc.max == 0 {
            bail!("repo {}: max must be at least 1", rc.repo);
        }
    }
    if raw.supervisor.service_log_max_mb == Some(0) {
        bail!("service_log_max_mb must be at least 1");
    }
    for monitor in &raw.monitors {
        if monitor.name.trim().is_empty() {
            bail!("[[monitors]] name must not be empty");
        }
        if monitor.duration_min == Some(0) {
            bail!("monitor {}: duration_min must be at least 1", monitor.name);
        }
        if monitor.consecutive_failures == Some(0) {
            bail!(
                "monitor {}: consecutive_failures must be at least 1",
                monitor.name
            );
        }
        if let Some(pattern) = &monitor.log_pattern {
            regex::Regex::new(pattern)
                .with_context(|| format!("monitor {}: invalid log_pattern", monitor.name))?;
        }
        if monitor.duration_min.is_none()
            && monitor.consecutive_failures.is_none()
            && !monitor.oom
            && monitor.log_pattern.is_none()
        {
            bail!("monitor {} has no trigger", monitor.name);
        }
    }

    let data_dir = expand_tilde(
        raw.supervisor
            .data_dir
            .as_deref()
            .unwrap_or("~/.local/share/homerunner"),
    );
    std::fs::create_dir_all(&data_dir)?;

    Ok(Config {
        dashboard_port: raw.supervisor.dashboard_port.unwrap_or(4123),
        max_total_runners: raw.supervisor.max_total_runners.unwrap_or(4),
        keep_failed_workspaces: raw.supervisor.keep_failed_workspaces.unwrap_or(2),
        failed_workspaces_max_age_days: raw.supervisor.failed_workspaces_max_age_days,
        failed_workspaces_max_bytes: raw
            .supervisor
            .failed_workspaces_max_mb
            .map(|mb| mb.saturating_mul(1024 * 1024)),
        keep_job_logs: raw.supervisor.keep_job_logs.unwrap_or(100),
        job_logs_max_age_days: raw.supervisor.job_logs_max_age_days,
        job_logs_max_bytes: raw
            .supervisor
            .job_logs_max_mb
            .map(|mb| mb.saturating_mul(1024 * 1024)),
        job_history_days: raw.supervisor.job_history_days.unwrap_or(365),
        event_history_days: raw.supervisor.event_history_days.unwrap_or(7),
        service_log_max_bytes: raw
            .supervisor
            .service_log_max_mb
            .unwrap_or(10)
            .saturating_mul(1024 * 1024),
        service_log_backups: raw.supervisor.service_log_backups.unwrap_or(3),
        poll_interval_s: raw.supervisor.poll_interval_s.unwrap_or(30),
        idle_decay_min: raw.supervisor.idle_decay_min.unwrap_or(10),
        data_dir,
        auth_source: raw.auth.source.unwrap_or_else(|| "gh".into()),
        python_version: raw.toolchains.python.unwrap_or_else(|| "3.13.1".into()),
        node_version: raw.toolchains.node.unwrap_or_else(|| "24.14.0".into()),
        repos,
        monitors: raw.monitors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn write_config(dir: &TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn toml_path(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn loads_defaults_and_repo_overrides() {
        let dir = TempDir::new("config-merge");
        let data_dir = dir.path().join("data");
        let path = write_config(
            &dir,
            &format!(
                r#"
[supervisor]
dashboard_port = 5000
max_total_runners = 7
data_dir = "{}"
keep_failed_workspaces = 4
failed_workspaces_max_age_days = 14
failed_workspaces_max_mb = 2048
keep_job_logs = 25
job_logs_max_age_days = 30
job_logs_max_mb = 512
job_history_days = 180
event_history_days = 5
service_log_max_mb = 8
service_log_backups = 4
poll_interval_s = 12
idle_decay_min = 3

[auth]
source = "env:TEST_TOKEN"

[toolchains]
python = "3.12"
node = "22"

[defaults]
runtime = "apple-container"
labels = ["self-hosted", "arm64"]
image = "runner:default"
reserved = 1
max = 3
job_timeout_min = 45
caffeinate = false
registry_mirror = "http://mirror"

[[repos]]
repo = "owner/first"

[[repos]]
repo = "owner/second"
runtime = "docker"
reserved = 2
image = "runner:custom"
"#,
                toml_path(&data_dir)
            ),
        );

        let config = load(&path).unwrap();
        assert_eq!(config.dashboard_port, 5000);
        assert_eq!(config.max_total_runners, 7);
        assert_eq!(config.keep_failed_workspaces, 4);
        assert_eq!(config.failed_workspaces_max_age_days, Some(14));
        assert_eq!(config.failed_workspaces_max_bytes, Some(2048 * 1024 * 1024));
        assert_eq!(config.keep_job_logs, 25);
        assert_eq!(config.job_logs_max_age_days, Some(30));
        assert_eq!(config.job_logs_max_bytes, Some(512 * 1024 * 1024));
        assert_eq!(config.job_history_days, 180);
        assert_eq!(config.event_history_days, 5);
        assert_eq!(config.service_log_max_bytes, 8 * 1024 * 1024);
        assert_eq!(config.service_log_backups, 4);
        assert_eq!(config.poll_interval_s, 12);
        assert_eq!(config.idle_decay_min, 3);
        assert_eq!(config.auth_source, "env:TEST_TOKEN");
        assert_eq!(config.python_version, "3.12");
        assert_eq!(config.node_version, "22");
        assert!(data_dir.is_dir());

        let first = &config.repos[0];
        assert_eq!(first.runtime, RuntimeKind::AppleContainer);
        assert_eq!(first.labels, ["self-hosted", "arm64"]);
        assert_eq!(first.image, "runner:default");
        assert_eq!(first.reserved, 1);
        assert_eq!(first.max, 3);
        assert_eq!(first.job_timeout_min, 45);
        assert!(!first.caffeinate);
        assert_eq!(first.registry_mirror.as_deref(), Some("http://mirror"));

        let second = &config.repos[1];
        assert_eq!(second.runtime, RuntimeKind::Docker);
        assert_eq!(second.reserved, 2);
        assert_eq!(second.max, 3);
        assert_eq!(second.image, "runner:custom");
    }

    #[test]
    fn supplies_documented_defaults() {
        let dir = TempDir::new("config-defaults");
        let data_dir = dir.path().join("data");
        let path = write_config(
            &dir,
            &format!(
                "[supervisor]\ndata_dir = \"{}\"\n[[repos]]\nrepo = \"owner/project\"\n",
                toml_path(&data_dir)
            ),
        );

        let config = load(&path).unwrap();
        assert_eq!(config.dashboard_port, 4123);
        assert_eq!(config.max_total_runners, 4);
        assert_eq!(config.keep_failed_workspaces, 2);
        assert_eq!(config.failed_workspaces_max_age_days, None);
        assert_eq!(config.failed_workspaces_max_bytes, None);
        assert_eq!(config.keep_job_logs, 100);
        assert_eq!(config.job_logs_max_age_days, None);
        assert_eq!(config.job_logs_max_bytes, None);
        assert_eq!(config.job_history_days, 365);
        assert_eq!(config.event_history_days, 7);
        assert_eq!(config.service_log_max_bytes, 10 * 1024 * 1024);
        assert_eq!(config.service_log_backups, 3);
        assert_eq!(config.repos[0].labels, ["self-hosted", "linux", "x64"]);
        assert_eq!(config.repos[0].reserved, 1);
        assert_eq!(config.repos[0].max, 2);
    }

    #[test]
    fn rejects_repo_without_owner() {
        let dir = TempDir::new("config-repo-name");
        let path = write_config(&dir, "[[repos]]\nrepo = \"project\"\n");
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("needs repo = \"owner/name\""));
    }

    #[test]
    fn rejects_max_below_reserved() {
        let dir = TempDir::new("config-max");
        let path = write_config(
            &dir,
            "[[repos]]\nrepo = \"owner/project\"\nreserved = 3\nmax = 2\n",
        );
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("max (2) must be >= reserved (3)"));
    }

    #[test]
    fn rejects_empty_repo_list() {
        let dir = TempDir::new("config-empty");
        let path = write_config(&dir, "[supervisor]\ndashboard_port = 5000\n");
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("no [[repos]] configured"));
    }

    #[test]
    fn loads_and_validates_monitors() {
        let dir = TempDir::new("config-monitors");
        let data_dir = dir.path().join("data");
        let path = write_config(
            &dir,
            &format!(
                r#"
[supervisor]
data_dir = "{}"

[[repos]]
repo = "owner/project"

[[monitors]]
name = "slow or stuck"
repo = "owner/project"
workflow = "CI"
duration_min = 15
consecutive_failures = 2
oom = true
log_pattern = "(?i)deadlock"
retain_workspace = true
"#,
                toml_path(&data_dir)
            ),
        );

        let config = load(&path).unwrap();
        assert_eq!(config.monitors.len(), 1);
        assert_eq!(config.monitors[0].duration_min, Some(15));
        assert!(config.monitors[0].retain_workspace);

        let invalid = write_config(
            &dir,
            "[[repos]]\nrepo = \"owner/project\"\n[[monitors]]\nname = \"bad\"\nlog_pattern = \"[\"\n",
        );
        assert!(load(&invalid)
            .unwrap_err()
            .to_string()
            .contains("invalid log_pattern"));
    }

    #[test]
    fn repo_slug_is_stable() {
        let repo = RepoConfig {
            repo: "owner/project".into(),
            runtime: RuntimeKind::Docker,
            labels: vec![],
            image: String::new(),
            reserved: 0,
            max: 1,
            job_timeout_min: 1,
            caffeinate: false,
            registry_mirror: None,
        };
        assert_eq!(repo.slug(), "owner-project");
    }

    #[test]
    fn test_config_paths_are_valid_toml_on_windows() {
        assert_eq!(
            toml_path(Path::new(r"C:\Users\runner\homerunner")),
            "C:/Users/runner/homerunner"
        );
    }
}
