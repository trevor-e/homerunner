//! Agent-facing queries over captured job state. Both the CLI verbs and the
//! MCP server are thin presentations of these — one source of truth.

use crate::store::{JobFilter, Store};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AnalyticsQuery {
    pub repo: Option<String>,
    pub workflow: Option<String>,
    pub job_name: Option<String>,
    pub since: Option<String>,
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn window_seconds(value: &str) -> Result<Option<f64>> {
    if value == "all" {
        return Ok(None);
    }
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .context("since must look like 24h, 7d, 4w, or 'all'")?;
    let amount: f64 = value[..split]
        .parse::<u64>()
        .context("since must start with a positive integer")? as f64;
    anyhow::ensure!(amount > 0.0, "since must be greater than zero");
    let unit = &value[split..];
    let multiplier = match unit {
        "m" => 60.0,
        "h" => 3_600.0,
        "d" => 86_400.0,
        "w" => 7.0 * 86_400.0,
        _ => bail!("since unit must be m, h, d, or w"),
    };
    Ok(Some(amount * multiplier))
}

fn percentile(values: &[f64], fraction: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let position = (values.len() - 1) as f64 * fraction;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        Some(values[lower])
    } else {
        let weight = position - lower as f64;
        Some(values[lower] * (1.0 - weight) + values[upper] * weight)
    }
}

fn duration(job: &Value) -> Option<f64> {
    let value = job["completed_at"].as_f64()? - job["started_at"].as_f64()?;
    (value >= 0.0).then_some(value)
}

/// Cross-run aggregates and evidence-backed tuning signals. Groups are always
/// repo/workflow/job because that is the stable unit a workflow author can act on.
pub fn analytics(store: &Store, query: &AnalyticsQuery) -> Result<Value> {
    let window = query.since.as_deref().unwrap_or("30d");
    let completed_after = window_seconds(window)?.map(|seconds| now() - seconds);
    let jobs = store.filtered_jobs(&JobFilter {
        repo: query.repo.as_deref(),
        workflow: query.workflow.as_deref(),
        job_name: query.job_name.as_deref(),
        completed_after,
    });
    let mut buckets: BTreeMap<(String, String, String), Vec<&Value>> = BTreeMap::new();
    for job in &jobs {
        buckets
            .entry((
                job["repo"].as_str().unwrap_or("unknown").to_string(),
                job["workflow"].as_str().unwrap_or("unknown").to_string(),
                job["job_name"].as_str().unwrap_or("unknown").to_string(),
            ))
            .or_default()
            .push(job);
    }

    let mut recommendations = Vec::new();
    let mut groups = Vec::new();
    for ((repo, workflow, job_name), runs) in buckets {
        let successes = runs
            .iter()
            .filter(|job| job["conclusion"].as_str() == Some("success"))
            .count();
        let failures = runs
            .iter()
            .filter(|job| job["conclusion"].as_str() == Some("failure"))
            .count();
        let decisive = successes + failures;
        let durations: Vec<f64> = runs.iter().filter_map(|job| duration(job)).collect();
        let memory: Vec<f64> = runs
            .iter()
            .filter_map(|job| job["peak_mem_mb"].as_f64())
            .collect();
        let cpu_avg: Vec<f64> = runs
            .iter()
            .filter_map(|job| job["cpu_avg_pct"].as_f64())
            .collect();
        let oom_count = runs
            .iter()
            .filter(|job| job["oom"].as_bool() == Some(true))
            .count();
        let p50 = percentile(&durations, 0.50);
        let p95 = percentile(&durations, 0.95);
        let target = format!("{repo} / {workflow} / {job_name}");
        let mut flags = Vec::new();

        if oom_count > 0 {
            flags.push("oom");
            recommendations.push(json!({
                "severity": "high", "kind": "oom", "target": target,
                "message": format!("{oom_count} run(s) were OOM-killed; increase memory or reduce peak usage."),
            }));
        }

        let ordered_durations: Vec<f64> = runs.iter().filter_map(|job| duration(job)).collect();
        if ordered_durations.len() >= 6 {
            let recent_count = ordered_durations.len().min(10) / 2;
            let recent = percentile(&ordered_durations[..recent_count], 0.50).unwrap_or(0.0);
            let baseline = percentile(&ordered_durations[recent_count..], 0.50).unwrap_or(0.0);
            if baseline > 0.0 && recent > baseline * 1.25 && recent - baseline >= 10.0 {
                flags.push("regression");
                recommendations.push(json!({
                    "severity": "medium", "kind": "duration_regression", "target": target,
                    "message": format!("Recent median is {:.0}s versus {:.0}s previously ({:.0}% slower).", recent, baseline, (recent / baseline - 1.0) * 100.0),
                }));
            }
        }
        if decisive >= 5 && successes > 0 && failures > 0 {
            flags.push("intermittent-failures");
            recommendations.push(json!({
                "severity": "medium", "kind": "intermittent_failures", "target": target,
                "message": format!("{failures} of {decisive} decisive runs failed; inspect this job for flaky behavior."),
            }));
        }
        if durations.len() >= 5
            && p50.is_some_and(|median| {
                p95.unwrap_or(median) > median * 2.0 && p95.unwrap_or(median) - median >= 30.0
            })
        {
            flags.push("slow-tail");
            recommendations.push(json!({
                "severity": "low", "kind": "slow_tail", "target": target,
                "message": format!("Duration p95 ({:.0}s) is more than twice p50 ({:.0}s); compare slow runs for contention or cold caches.", p95.unwrap(), p50.unwrap()),
            }));
        }
        let cpu_p95 = percentile(&cpu_avg, 0.95);
        if cpu_avg.len() >= 5
            && p50.is_some_and(|median| median >= 60.0)
            && cpu_p95.is_some_and(|cpu| cpu < 30.0)
        {
            flags.push("low-cpu");
            recommendations.push(json!({
                "severity": "low", "kind": "low_cpu", "target": target,
                "message": format!("CPU average p95 is {:.0}% while median duration is {:.0}s; investigate network, cache, or lock waits before adding CPU.", cpu_p95.unwrap(), p50.unwrap()),
            }));
        }

        groups.push(json!({
            "repo": repo,
            "workflow": workflow,
            "job_name": job_name,
            "runs": runs.len(),
            "successes": successes,
            "failures": failures,
            "pass_rate_pct": (decisive > 0).then(|| successes as f64 / decisive as f64 * 100.0),
            "duration_s": {
                "p50": p50,
                "p95": p95,
                "max": durations.iter().copied().reduce(f64::max),
            },
            "peak_mem_mb": {
                "p50": percentile(&memory, 0.50),
                "p95": percentile(&memory, 0.95),
                "max": memory.iter().copied().reduce(f64::max),
            },
            "cpu_avg_pct": {
                "p50": percentile(&cpu_avg, 0.50),
                "p95": cpu_p95,
                "max": cpu_avg.iter().copied().reduce(f64::max),
            },
            "oom_count": oom_count,
            "flags": flags,
        }));
    }
    groups.sort_by(|a, b| {
        b["duration_s"]["p95"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["duration_s"]["p95"].as_f64().unwrap_or(0.0))
    });
    recommendations.sort_by_key(|item| match item["severity"].as_str() {
        Some("high") => 0,
        Some("medium") => 1,
        _ => 2,
    });
    let successes = jobs
        .iter()
        .filter(|job| job["conclusion"].as_str() == Some("success"))
        .count();
    let failures = jobs
        .iter()
        .filter(|job| job["conclusion"].as_str() == Some("failure"))
        .count();
    let decisive = successes + failures;
    Ok(json!({
        "window": window,
        "filters": {
            "repo": query.repo,
            "workflow": query.workflow,
            "job_name": query.job_name,
        },
        "summary": {
            "runs": jobs.len(),
            "successes": successes,
            "failures": failures,
            "pass_rate_pct": (decisive > 0).then(|| successes as f64 / decisive as f64 * 100.0),
            "oom_count": jobs.iter().filter(|job| job["oom"].as_bool() == Some(true)).count(),
        },
        "groups": groups,
        "recommendations": recommendations,
    }))
}

fn short_duration(value: Option<f64>) -> String {
    let Some(seconds) = value else {
        return "—".into();
    };
    if seconds >= 3_600.0 {
        format!("{:.1}h", seconds / 3_600.0)
    } else if seconds >= 60.0 {
        format!("{:.1}m", seconds / 60.0)
    } else {
        format!("{seconds:.0}s")
    }
}

pub fn analytics_text(report: &Value) -> String {
    let summary = &report["summary"];
    let mut out = format!(
        "CI analytics ({}) — {} runs, {} passed, {} failed, {} OOM\n\n",
        report["window"].as_str().unwrap_or("all"),
        summary["runs"].as_u64().unwrap_or(0),
        summary["successes"].as_u64().unwrap_or(0),
        summary["failures"].as_u64().unwrap_or(0),
        summary["oom_count"].as_u64().unwrap_or(0),
    );
    out.push_str(&format!(
        "{:<24} {:<20} {:<24} {:>5} {:>6} {:>7} {:>7} {:>8} {:>9}  {}\n",
        "REPOSITORY",
        "WORKFLOW",
        "JOB",
        "RUNS",
        "PASS",
        "P50",
        "P95",
        "CPU P95",
        "MEM P95",
        "SIGNALS"
    ));
    for group in report["groups"].as_array().into_iter().flatten() {
        let flags = group["flags"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let pass = group["pass_rate_pct"]
            .as_f64()
            .map(|value| format!("{value:.0}%"))
            .unwrap_or_else(|| "—".into());
        let mem = group["peak_mem_mb"]["p95"]
            .as_f64()
            .map(|value| format!("{value:.0}M"))
            .unwrap_or_else(|| "—".into());
        let cpu = group["cpu_avg_pct"]["p95"]
            .as_f64()
            .map(|value| format!("{value:.0}%"))
            .unwrap_or_else(|| "—".into());
        out.push_str(&format!(
            "{:<24.24} {:<20.20} {:<24.24} {:>5} {:>6} {:>7} {:>7} {:>8} {:>9}  {}\n",
            group["repo"].as_str().unwrap_or(""),
            group["workflow"].as_str().unwrap_or(""),
            group["job_name"].as_str().unwrap_or(""),
            group["runs"].as_u64().unwrap_or(0),
            pass,
            short_duration(group["duration_s"]["p50"].as_f64()),
            short_duration(group["duration_s"]["p95"].as_f64()),
            cpu,
            mem,
            flags,
        ));
    }
    if report["groups"]
        .as_array()
        .is_none_or(|items| items.is_empty())
    {
        out.push_str("no completed jobs match these filters\n");
    }
    if let Some(items) = report["recommendations"]
        .as_array()
        .filter(|items| !items.is_empty())
    {
        out.push_str("\nRecommendations\n");
        for item in items {
            out.push_str(&format!(
                "[{}] {}: {}\n",
                item["severity"].as_str().unwrap_or("low"),
                item["target"].as_str().unwrap_or("job"),
                item["message"].as_str().unwrap_or(""),
            ));
        }
    }
    out
}

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
            store
                .job(id)
                .with_context(|| format!("no job {id} recorded"))
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
        "post_mortem": job["kept_image"].as_str().map(|_| format!("workspace kept — `homerunner exec {key}` opens a shell in it; `homerunner exec {key} -- <cmd>` runs one command without a TTY")),
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
    if let Some(sha) = job["head_sha"].as_str() {
        out.push_str(&format!(
            "{} @ {} ({}) — {}\n",
            job["head_branch"].as_str().unwrap_or("?"),
            &sha[..sha.len().min(9)],
            job["event"].as_str().unwrap_or("?"),
            job["title"].as_str().unwrap_or(""),
        ));
    }
    if let Some(peak) = job["peak_mem_mb"].as_f64() {
        out.push_str(&format!(
            "peak memory: {peak:.0} MB{}\n",
            if job["oom"].as_bool() == Some(true) {
                " — OOM-KILLED"
            } else {
                ""
            }
        ));
    }
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
            j["gh_job_id"]
                .as_i64()
                .map(|i| i.to_string())
                .unwrap_or_default(),
            j["conclusion"].as_str().unwrap_or("running"),
            j["repo"].as_str().unwrap_or(""),
            j["job_name"].as_str().unwrap_or(""),
            dur,
            if j["log_dir"].is_string() { "logs" } else { "" },
            if j["kept_image"].is_string() {
                "+workspace"
            } else {
                ""
            },
        ));
    }
    if out.is_empty() {
        out.push_str("no jobs recorded\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::path::Path;

    #[test]
    fn worker_logs_are_filtered_and_joined_in_filename_order() {
        let dir = TempDir::new("worker-logs");
        let diag = dir.path().join("diag");
        std::fs::create_dir(&diag).unwrap();
        std::fs::write(diag.join("Worker_002.log"), "second\n").unwrap();
        std::fs::write(diag.join("Worker_001.log"), "first\n").unwrap();
        std::fs::write(diag.join("Runner_001.log"), "ignored\n").unwrap();

        let job = json!({"log_dir": dir.path().to_string_lossy()});
        assert_eq!(read_worker_logs(&job).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn worker_logs_explain_missing_capture() {
        let error = read_worker_logs(&json!({})).unwrap_err().to_string();
        assert!(error.contains("no captured logs"));
    }

    #[test]
    fn failure_excerpt_centers_on_the_last_error() {
        let log = (0..=100)
            .map(|i| match i {
                70 => "line 70 ##[error] first".into(),
                90 => "line 90 Process completed with exit code 1".into(),
                _ => format!("line {i}"),
            })
            .collect::<Vec<_>>()
            .join("\n");

        let excerpt = failure_excerpt(&log);
        assert!(excerpt.starts_with("line 60\n"));
        assert!(excerpt.contains("line 90 Process completed with exit code 1"));
        assert!(excerpt.ends_with("line 99"));
        assert!(!excerpt.contains("line 59\n"));
        assert!(!excerpt.contains("line 100"));
    }

    #[test]
    fn failure_excerpt_falls_back_to_last_forty_lines() {
        let log = (0..50)
            .map(|i| format!("ordinary line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let excerpt = failure_excerpt(&log);

        assert_eq!(excerpt.lines().count(), 40);
        assert!(excerpt.starts_with("ordinary line 10\n"));
        assert!(excerpt.ends_with("ordinary line 49"));
    }

    #[test]
    fn resolve_job_supports_ids_and_reports_bad_specs() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let info = json!({"job_id": 42, "job_name": "tests"});
        store.job_started(&info, "owner/repo", "runner", Some(1.0));

        assert_eq!(
            resolve_job(&store, Some("42"), false).unwrap()["gh_job_id"],
            42
        );
        assert!(resolve_job(&store, Some("not-an-id"), false)
            .unwrap_err()
            .to_string()
            .contains("job must be a numeric id"));
        assert!(resolve_job(&store, Some("99"), false)
            .unwrap_err()
            .to_string()
            .contains("no job 99 recorded"));
    }

    #[test]
    fn jobs_table_includes_duration_and_artifact_state() {
        let output = jobs_table(&[json!({
            "gh_job_id": 42,
            "conclusion": "failure",
            "repo": "owner/repo",
            "job_name": "tests",
            "started_at": 10.0,
            "completed_at": 75.6,
            "log_dir": "/logs/42",
            "kept_image": "kept:42",
        })]);

        assert!(output.contains("66s"));
        assert!(output.contains("logs+workspace"));
        assert_eq!(jobs_table(&[]), "no jobs recorded\n");
    }

    #[test]
    fn why_text_surfaces_resource_and_post_mortem_details() {
        let output = why_text(&json!({
            "job": {
                "repo": "owner/repo",
                "workflow": "CI",
                "job_name": "tests",
                "conclusion": "failure",
                "html_url": "https://example.test/job/42",
                "head_sha": "123456789abcdef",
                "head_branch": "main",
                "event": "push",
                "title": "A change",
                "peak_mem_mb": 512.0,
                "oom": true,
            },
            "post_mortem": "workspace kept",
            "excerpt": "the failure",
        }));

        assert!(output.contains("main @ 123456789 (push)"));
        assert!(output.contains("peak memory: 512 MB — OOM-KILLED"));
        assert!(output.contains("workspace kept"));
        assert!(output.contains("the failure"));
    }

    #[test]
    fn analytics_groups_runs_and_emits_actionable_signals() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        for (id, seconds, conclusion) in [
            (1, 10.0, "success"),
            (2, 20.0, "success"),
            (3, 30.0, "failure"),
            (4, 40.0, "success"),
            (5, 50.0, "failure"),
        ] {
            let info = json!({
                "job_id": id,
                "workflow": "CI",
                "job_name": "tests",
            });
            store.job_started(&info, "owner/repo", "runner", Some(now() - seconds));
            store.job_concluded(id, conclusion);
        }
        store.set_job_resources(3, 900.0, true, Some(160.0), Some(220.0));

        let report = analytics(&store, &AnalyticsQuery::default()).unwrap();
        assert_eq!(report["summary"]["runs"], 5);
        assert_eq!(report["summary"]["pass_rate_pct"], 60.0);
        assert!((report["groups"][0]["duration_s"]["p50"].as_f64().unwrap() - 30.0).abs() < 0.1);
        assert_eq!(report["groups"][0]["oom_count"], 1);
        assert_eq!(report["groups"][0]["cpu_avg_pct"]["p95"], 160.0);
        assert!(report["recommendations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "oom"));
        let text = analytics_text(&report);
        assert!(text.contains("owner/repo"));
        assert!(text.contains("intermittent-failures"));
        assert!(text.contains("Recommendations"));
    }

    #[test]
    fn analytics_window_parser_rejects_ambiguous_values() {
        assert_eq!(window_seconds("7d").unwrap(), Some(604_800.0));
        assert_eq!(window_seconds("all").unwrap(), None);
        assert!(window_seconds("7days").is_err());
        assert!(window_seconds("0h").is_err());
    }
}
