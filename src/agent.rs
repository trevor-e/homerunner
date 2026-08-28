//! Agent-facing queries over captured job state. Both the CLI verbs and the
//! MCP server are thin presentations of these — one source of truth.

use crate::store::Store;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Resolve "1234", "latest", "latest-failed", or None (-> latest-failed when
/// `default_failed`, else latest) against the job journal.
pub fn resolve_job(store: &Store, spec: Option<&str>, default_failed: bool) -> Result<Value> {
    match spec {
        None => {
            if default_failed {
                store.latest_job(true).context("no failed jobs recorded")
            } else {
                store.latest_job(false).context("no jobs recorded")
            }
        }
        Some("latest") => store.latest_job(false).context("no jobs recorded"),
        Some("latest-failed") => store.latest_job(true).context("no failed jobs recorded"),
        Some(s) => {
            let id: i64 = s
                .parse()
                .context("job must be a numeric id, 'latest', or 'latest-failed'")?;
            store.job(id).with_context(|| format!("no job {id} recorded"))
        }
    }
}

/// The runner's Worker_*.log files carry per-step execution detail; they're
/// captured from /home/runner/_diag at reap time.
pub fn read_worker_logs(job: &Value) -> Result<String> {
    let dir = job["log_dir"]
        .as_str()
        .context("no captured logs for this job (it may predate log capture)")?;
    let diag = std::path::Path::new(dir).join("diag");
    let mut files: Vec<_> = std::fs::read_dir(&diag)
        .with_context(|| format!("log dir missing: {}", diag.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("Worker_"))
        })
        .collect();
    files.sort();
    if files.is_empty() {
        bail!("no Worker logs captured in {}", diag.display());
    }
    let mut out = String::new();
    for f in files {
        out.push_str(&std::fs::read_to_string(&f).unwrap_or_default());
    }
    Ok(out)
}

/// The interesting slice of a failed job's log: context around the last
/// error marker, or the tail if no marker is found.
pub fn failure_excerpt(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    let marker = lines
        .iter()
        .rposition(|l| l.contains("##[error") || l.contains("Process completed with exit code"));
    let (from, to) = match marker {
        Some(i) => (i.saturating_sub(30), (i + 10).min(lines.len())),
        None => (lines.len().saturating_sub(40), lines.len()),
    };
    lines[from..to].join("\n")
}

pub fn why(store: &Store, spec: Option<&str>) -> Result<Value> {
    let job = resolve_job(store, spec, true)?;
    let excerpt = read_worker_logs(&job).map(|l| failure_excerpt(&l)).ok();
    let key = job["gh_job_id"]
        .as_i64()
        .map(|i| i.to_string())
        .unwrap_or_default();
    Ok(json!({
        "job": job,
        "excerpt": excerpt,
        "post_mortem": job["kept_image"].as_str().map(|_| format!("workspace kept — `homerunner exec {key}` opens a shell in it")),
    }))
}

pub fn why_text(digest: &Value) -> String {
    let job = &digest["job"];
    let mut out = format!(
        "{} / {} / {} — {}\n{}\n",
        job["repo"].as_str().unwrap_or("?"),
        job["workflow"].as_str().unwrap_or("?"),
        job["job_name"].as_str().unwrap_or("?"),
        job["conclusion"].as_str().unwrap_or("unknown"),
        job["html_url"].as_str().unwrap_or(""),
    );
    if let Some(pm) = digest["post_mortem"].as_str() {
        out.push_str(pm);
        out.push('\n');
    }
    match digest["excerpt"].as_str() {
        Some(e) => {
            out.push_str("\n--- log excerpt around the failure ---\n");
            out.push_str(e);
            out.push('\n');
        }
        None => out.push_str("(no captured logs for this job)\n"),
    }
    out
}

pub fn jobs_table(jobs: &[Value]) -> String {
    let mut out = String::new();
    for j in jobs {
        let dur = match (j["started_at"].as_f64(), j["completed_at"].as_f64()) {
            (Some(s), Some(c)) => format!("{}s", (c - s).round() as i64),
            _ => String::new(),
        };
        out.push_str(&format!(
            "{:<12} {:<9} {:<28} {:<24} {:>6}  {}{}\n",
            j["gh_job_id"].as_i64().map(|i| i.to_string()).unwrap_or_default(),
            j["conclusion"].as_str().unwrap_or("running"),
            j["repo"].as_str().unwrap_or(""),
            j["job_name"].as_str().unwrap_or(""),
            dur,
            if j["log_dir"].is_string() { "logs" } else { "" },
            if j["kept_image"].is_string() { "+workspace" } else { "" },
        ));
    }
    if out.is_empty() {
        out.push_str("no jobs recorded\n");
    }
    out
}
