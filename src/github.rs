//! GitHub REST client. Every call is event-driven (spawn, busy transition,
//! startup sweep, daily staleness check) — the supervisor never polls for
//! work; runners hear about jobs over their own long-poll to GitHub's broker.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const API: &str = "https://api.github.com";

pub struct GitHub {
    client: reqwest::Client,
    auth_source: String,
    token: Mutex<Option<String>>,
}

async fn resolve_token(source: &str) -> Result<String> {
    if source == "gh" {
        let out = tokio::process::Command::new("gh")
            .args(["auth", "token"])
            .output()
            .await
            .context("failed to run `gh auth token`")?;
        if !out.status.success() {
            bail!(
                "`gh auth token` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    if let Some(var) = source.strip_prefix("env:") {
        return std::env::var(var).map_err(|_| anyhow!("env var {var} is empty"));
    }
    if let Some(path) = source.strip_prefix("file:") {
        let path = crate::config::expand_tilde(path);
        return Ok(std::fs::read_to_string(path)?.trim().to_string());
    }
    bail!("unknown auth source: {source}")
}

impl GitHub {
    pub fn new(auth_source: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("homerunner")
                .build()
                .expect("reqwest client"),
            auth_source: auth_source.to_string(),
            token: Mutex::new(None),
        }
    }

    async fn token(&self) -> Result<String> {
        let mut guard = self.token.lock().await;
        if guard.is_none() {
            *guard = Some(resolve_token(&self.auth_source).await?);
        }
        Ok(guard.clone().unwrap())
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        for attempt in 0..2 {
            let token = self.token().await?;
            let mut req = self
                .client
                .request(method.clone(), format!("{API}{url}"))
                .bearer_auth(&token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            if let Some(ref b) = body {
                req = req.json(b);
            }
            let resp = req.send().await?;
            let status = resp.status();
            // Token may have been rotated (e.g. `gh auth refresh`) — re-resolve once.
            if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                *self.token.lock().await = None;
                continue;
            }
            if status == reqwest::StatusCode::NO_CONTENT {
                return Ok(Value::Null);
            }
            let value: Value = resp.json().await.unwrap_or(Value::Null);
            if status.is_client_error() || status.is_server_error() {
                bail!("{method} {url} -> {status}: {value}");
            }
            return Ok(value);
        }
        unreachable!()
    }

    async fn get(&self, url: &str) -> Result<Value> {
        self.request(reqwest::Method::GET, url, None).await
    }

    /// Returns (runner_id, encoded_jit_config). The encoded config is
    /// credential material — pass it to the container env, never log it.
    pub async fn generate_jitconfig(
        &self,
        repo: &str,
        name: &str,
        labels: &[String],
    ) -> Result<(i64, String)> {
        let body = self
            .request(
                reqwest::Method::POST,
                &format!("/repos/{repo}/actions/runners/generate-jitconfig"),
                Some(json!({
                    "name": name,
                    // personal repos only have the default group
                    "runner_group_id": 1,
                    "labels": labels,
                    "work_folder": "_work",
                })),
            )
            .await?;
        let id = body["runner"]["id"].as_i64().context("no runner id")?;
        let cfg = body["encoded_jit_config"]
            .as_str()
            .context("no jit config")?;
        Ok((id, cfg.to_string()))
    }

    pub async fn list_runners(&self, repo: &str) -> Result<Vec<Value>> {
        let body = self
            .get(&format!("/repos/{repo}/actions/runners?per_page=100"))
            .await?;
        Ok(body["runners"].as_array().cloned().unwrap_or_default())
    }

    pub async fn delete_runner(&self, repo: &str, runner_id: i64) -> Result<()> {
        self.request(
            reqwest::Method::DELETE,
            &format!("/repos/{repo}/actions/runners/{runner_id}"),
            None,
        )
        .await?;
        Ok(())
    }

    /// Job enrichment after a runner turns busy: find the job assigned to it.
    /// Checks in-progress runs first, then recently-completed ones — a short
    /// job can finish before the API ever showed its run as in_progress.
    pub async fn find_job_by_runner(&self, repo: &str, runner_name: &str) -> Result<Option<Value>> {
        for status in ["in_progress", "completed"] {
            let runs = self
                .get(&format!(
                    "/repos/{repo}/actions/runs?status={status}&per_page=8"
                ))
                .await?;
            for run in runs["workflow_runs"].as_array().into_iter().flatten() {
                let run_id = run["id"].as_i64().unwrap_or_default();
                let jobs = self
                    .get(&format!(
                        "/repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"
                    ))
                    .await?;
                for job in jobs["jobs"].as_array().into_iter().flatten() {
                    if job["runner_name"].as_str() == Some(runner_name) {
                        return Ok(Some(json!({
                            "job_id": job["id"],
                            "run_id": run_id,
                            "workflow": run["name"],
                            "job_name": job["name"],
                            "html_url": job["html_url"],
                            "head_branch": run["head_branch"],
                            "head_sha": run["head_sha"],
                            "title": run["display_title"],
                            "event": run["event"],
                        })));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Count queued jobs whose labels our runners satisfy. A run is only
    /// `queued` until its first job starts, so in_progress runs are scanned
    /// too. Used by the burst poller, never the warm path.
    pub async fn count_queued_jobs(&self, repo: &str, our_labels: &[String]) -> Result<u32> {
        let mut count = 0u32;
        for status in ["queued", "in_progress"] {
            let runs = self
                .get(&format!(
                    "/repos/{repo}/actions/runs?status={status}&per_page=10"
                ))
                .await?;
            for run in runs["workflow_runs"].as_array().into_iter().flatten() {
                let run_id = run["id"].as_i64().unwrap_or_default();
                let jobs = self
                    .get(&format!(
                        "/repos/{repo}/actions/runs/{run_id}/jobs?filter=latest&per_page=100"
                    ))
                    .await?;
                for job in jobs["jobs"].as_array().into_iter().flatten() {
                    let wants: Vec<&str> = job["labels"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|l| l.as_str())
                        .collect();
                    if job["status"].as_str() == Some("queued")
                        && !wants.is_empty()
                        && wants.iter().all(|l| our_labels.iter().any(|o| o == l))
                    {
                        count += 1;
                    }
                }
            }
        }
        Ok(count)
    }

    pub async fn latest_runner_release(&self) -> Result<String> {
        let body = self.get("/repos/actions/runner/releases/latest").await?;
        Ok(body["tag_name"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches('v')
            .to_string())
    }

    pub async fn rate_limit(&self) -> Result<Value> {
        let body = self.get("/rate_limit").await?;
        Ok(body["resources"]["core"].clone())
    }
}
