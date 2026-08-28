//! Config loading: TOML -> structs, with [defaults] merged into each repo.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
    pool_size: Option<u32>,
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
}

#[derive(Debug, Clone, Deserialize, Default)]
struct AuthSection {
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    supervisor: SupervisorSection,
    #[serde(default)]
    auth: AuthSection,
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
    pub pool_size: u32,
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
    pub auth_source: String,
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
            bail!("[[repos]] entry needs repo = \"owner/name\", got: {}", entry.repo);
        }
        let o = &entry.overrides;
        repos.push(RepoConfig {
            repo: entry.repo.clone(),
            runtime: o.runtime.or(d.runtime).unwrap_or(RuntimeKind::Docker),
            labels: o
                .labels
                .clone()
                .or_else(|| d.labels.clone())
                .unwrap_or_else(|| {
                    vec!["self-hosted".into(), "linux".into(), "x64".into()]
                }),
            image: o
                .image
                .clone()
                .or_else(|| d.image.clone())
                .unwrap_or_else(|| "homerunner-runner:local".into()),
            pool_size: o.pool_size.or(d.pool_size).unwrap_or(1),
            job_timeout_min: o.job_timeout_min.or(d.job_timeout_min).unwrap_or(120),
            caffeinate: o.caffeinate.or(d.caffeinate).unwrap_or(true),
            registry_mirror: o.registry_mirror.clone().or_else(|| d.registry_mirror.clone()),
        });
    }
    if repos.is_empty() {
        bail!("no [[repos]] configured");
    }

    let data_dir = expand_tilde(
        raw.supervisor
            .data_dir
            .as_deref()
            .unwrap_or("~/.local/share/homerunner"),
    );
    std::fs::create_dir_all(&data_dir)?;

    Ok(Config {
        dashboard_port: raw.supervisor.dashboard_port.unwrap_or(8123),
        max_total_runners: raw.supervisor.max_total_runners.unwrap_or(4),
        keep_failed_workspaces: raw.supervisor.keep_failed_workspaces.unwrap_or(2),
        data_dir,
        auth_source: raw.auth.source.unwrap_or_else(|| "gh".into()),
        repos,
    })
}
