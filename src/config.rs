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
    keep_job_logs: Option<u32>,
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
    /// How many jobs' captured log dirs to keep (oldest pruned at reap).
    pub keep_job_logs: u32,
    /// Queued-jobs poll cadence for repos that can burst (max > reserved).
    pub poll_interval_s: u64,
    /// Minutes an idle burst runner (beyond reserved) lives before decay.
    pub idle_decay_min: u64,
    pub auth_source: String,
    pub python_version: String,
    pub node_version: String,
    pub repos: Vec<RepoConfig>,
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
        keep_job_logs: raw.supervisor.keep_job_logs.unwrap_or(100),
        poll_interval_s: raw.supervisor.poll_interval_s.unwrap_or(30),
        idle_decay_min: raw.supervisor.idle_decay_min.unwrap_or(10),
        data_dir,
        auth_source: raw.auth.source.unwrap_or_else(|| "gh".into()),
        python_version: raw.toolchains.python.unwrap_or_else(|| "3.13.1".into()),
        node_version: raw.toolchains.node.unwrap_or_else(|| "24.14.0".into()),
        repos,
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
keep_job_logs = 25
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
                data_dir.display()
            ),
        );

        let config = load(&path).unwrap();
        assert_eq!(config.dashboard_port, 5000);
        assert_eq!(config.max_total_runners, 7);
        assert_eq!(config.keep_failed_workspaces, 4);
        assert_eq!(config.keep_job_logs, 25);
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
                data_dir.display()
            ),
        );

        let config = load(&path).unwrap();
        assert_eq!(config.dashboard_port, 4123);
        assert_eq!(config.max_total_runners, 4);
        assert_eq!(config.keep_failed_workspaces, 2);
        assert_eq!(config.keep_job_logs, 100);
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
}
