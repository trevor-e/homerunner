//! Structured views over captured runner diagnostics. The on-disk log budget
//! is intentionally small, so scanning retained files keeps the index simple
//! and guarantees search results disappear when their source logs are pruned.

use crate::agent;
use crate::store::Store;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub repo: Option<String>,
    pub workflow: Option<String>,
    pub job_name: Option<String>,
    pub branch: Option<String>,
    pub step: Option<String>,
    pub level: Option<String>,
    pub since: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedLine {
    line_no: usize,
    step_name: Option<String>,
    level: &'static str,
    message: String,
}

#[derive(Debug, Default)]
struct QueryParts {
    include: Vec<String>,
    exclude: Vec<String>,
    properties: Vec<(String, String, bool)>,
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
    let amount = value[..split]
        .parse::<u64>()
        .context("since must start with a positive integer")?;
    anyhow::ensure!(amount > 0, "since must be greater than zero");
    let multiplier = match &value[split..] {
        "m" => 60.0,
        "h" => 3_600.0,
        "d" => 86_400.0,
        "w" => 7.0 * 86_400.0,
        _ => bail!("since unit must be m, h, d, or w"),
    };
    Ok(Some(amount as f64 * multiplier))
}

fn tokens(input: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if ch.is_whitespace() && !quoted {
            if !token.is_empty() {
                out.push(std::mem::take(&mut token));
            }
        } else {
            token.push(ch);
        }
    }
    anyhow::ensure!(!quoted, "unterminated quote in log query");
    if escaped {
        token.push('\\');
    }
    if !token.is_empty() {
        out.push(token);
    }
    Ok(out)
}

fn parse_query(input: &str) -> Result<QueryParts> {
    let mut parts = QueryParts::default();
    for mut token in tokens(input)? {
        if token.eq_ignore_ascii_case("AND") {
            continue;
        }
        let negated = token.starts_with('-');
        if negated {
            token.remove(0);
        }
        if let Some((key, value)) = token.split_once(':') {
            let key = key.to_ascii_lowercase();
            if matches!(
                key.as_str(),
                "repo" | "workflow" | "job" | "job_name" | "branch" | "step" | "level"
            ) {
                for value in value.split(',').filter(|value| !value.is_empty()) {
                    parts
                        .properties
                        .push((key.clone(), value.to_string(), negated));
                }
                continue;
            }
        }
        if negated {
            parts.exclude.push(token.to_ascii_lowercase());
        } else if !token.is_empty() {
            parts.include.push(token.to_ascii_lowercase());
        }
    }
    Ok(parts)
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let value = value.to_ascii_lowercase();
    if !pattern.contains('*') {
        return value.contains(&pattern);
    }
    let anchored_start = !pattern.starts_with('*');
    let anchored_end = !pattern.ends_with('*');
    let mut cursor = 0;
    for (index, part) in pattern
        .split('*')
        .filter(|part| !part.is_empty())
        .enumerate()
    {
        let Some(offset) = value[cursor..].find(part) else {
            return false;
        };
        if index == 0 && anchored_start && offset != 0 {
            return false;
        }
        cursor += offset + part.len();
    }
    !anchored_end
        || pattern
            .rsplit('*')
            .next()
            .is_none_or(|tail| value.ends_with(tail))
}

fn property<'a>(job: &'a Value, line: Option<&'a ParsedLine>, key: &str) -> &'a str {
    match key {
        "repo" => job["repo"].as_str().unwrap_or(""),
        "workflow" => job["workflow"].as_str().unwrap_or(""),
        "job" | "job_name" => job["job_name"].as_str().unwrap_or(""),
        "branch" => job["head_branch"].as_str().unwrap_or(""),
        "step" => line
            .and_then(|line| line.step_name.as_deref())
            .unwrap_or(""),
        "level" => line.map(|line| line.level).unwrap_or(""),
        _ => "",
    }
}

fn properties_match(job: &Value, line: Option<&ParsedLine>, parts: &QueryParts) -> bool {
    parts.properties.iter().all(|(key, pattern, negated)| {
        let matched = wildcard_match(pattern, property(job, line, key));
        if *negated {
            !matched
        } else {
            parts
                .properties
                .iter()
                .filter(|(candidate, _, excluded)| candidate == key && !*excluded)
                .any(|(_, candidate, _)| wildcard_match(candidate, property(job, line, key)))
        }
    })
}

fn classify(line: &str) -> &'static str {
    let line = line.to_ascii_lowercase();
    if line.contains("##[error")
        || [" error", "fatal", "failed", "failure", "exception", "panic"]
            .iter()
            .any(|marker| line.contains(marker))
    {
        "error"
    } else if line.contains("##[warning") || line.contains("warn") {
        "warn"
    } else if line.contains("debug") {
        "debug"
    } else {
        "info"
    }
}

fn after_marker(line: &str, marker: &str) -> Option<String> {
    line.find(marker)
        .map(|index| line[index + marker.len()..].trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_lines(log: &str) -> Vec<ParsedLine> {
    let mut named_step: Option<String> = None;
    let mut groups: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (index, line) in log.lines().enumerate() {
        if let Some(step) = after_marker(line, "Starting: ") {
            named_step = Some(step);
        }
        if let Some(group) = after_marker(line, "##[group]") {
            groups.push(group);
        }
        out.push(ParsedLine {
            line_no: index + 1,
            step_name: named_step.clone().or_else(|| groups.first().cloned()),
            level: classify(line),
            message: line.to_string(),
        });
        if line.contains("##[endgroup]") {
            groups.pop();
        }
        if line.contains("Finishing: ") {
            named_step = None;
        }
    }
    out
}

fn add_explicit_properties(parts: &mut QueryParts, query: &SearchQuery) {
    for (key, value) in [
        ("repo", query.repo.as_deref()),
        ("workflow", query.workflow.as_deref()),
        ("job_name", query.job_name.as_deref()),
        ("branch", query.branch.as_deref()),
        ("step", query.step.as_deref()),
        ("level", query.level.as_deref()),
    ] {
        if let Some(value) = value {
            parts
                .properties
                .push((key.to_string(), value.to_string(), false));
        }
    }
}

pub fn search(store: &Store, query: &SearchQuery) -> Result<Value> {
    let mut parts = parse_query(query.q.as_deref().unwrap_or(""))?;
    add_explicit_properties(&mut parts, query);
    let window = query.since.as_deref().unwrap_or("30d");
    let completed_after = window_seconds(window)?.map(|seconds| now() - seconds);
    let limit = query.limit.unwrap_or(100).clamp(1, 1_000);
    let mut results = Vec::new();
    let mut scanned_jobs = 0;
    let mut total_matches = 0;

    for job in store.jobs_with_logs(10_000) {
        if completed_after
            .is_some_and(|cutoff| job["completed_at"].as_f64().unwrap_or(0.0) < cutoff)
            || !properties_match(
                &job,
                None,
                &QueryParts {
                    properties: parts
                        .properties
                        .iter()
                        .filter(|(key, _, _)| key != "step" && key != "level")
                        .cloned()
                        .collect(),
                    ..QueryParts::default()
                },
            )
        {
            continue;
        }
        let Ok(log) = agent::read_worker_logs(&job) else {
            continue;
        };
        scanned_jobs += 1;
        for line in parse_lines(&log) {
            if !properties_match(&job, Some(&line), &parts) {
                continue;
            }
            let message = line.message.to_ascii_lowercase();
            if !parts.include.iter().all(|term| message.contains(term))
                || parts.exclude.iter().any(|term| message.contains(term))
            {
                continue;
            }
            total_matches += 1;
            if results.len() < limit {
                results.push(json!({
                    "gh_job_id": job["gh_job_id"],
                    "repo": job["repo"],
                    "workflow": job["workflow"],
                    "job_name": job["job_name"],
                    "branch": job["head_branch"],
                    "conclusion": job["conclusion"],
                    "completed_at": job["completed_at"],
                    "html_url": job["html_url"],
                    "line_no": line.line_no,
                    "step_name": line.step_name,
                    "level": line.level,
                    "message": line.message,
                }));
            }
        }
    }
    Ok(json!({
        "query": query.q,
        "window": window,
        "scanned_jobs": scanned_jobs,
        "matches": total_matches,
        "truncated": total_matches > results.len(),
        "results": results,
    }))
}

fn values(set: BTreeSet<String>) -> Value {
    Value::Array(set.into_iter().take(200).map(Value::String).collect())
}

/// Values that make the log query language discoverable in the dashboard.
/// Only jobs whose captured Worker logs can still be read are included.
pub fn suggestions(store: &Store) -> Value {
    let mut repos = BTreeSet::new();
    let mut workflows = BTreeSet::new();
    let mut job_names = BTreeSet::new();
    let mut branches = BTreeSet::new();
    let mut steps = BTreeSet::new();
    let mut indexed_jobs = 0usize;
    let mut indexed_lines = 0usize;

    for job in store.jobs_with_logs(10_000) {
        let Ok(log) = agent::read_worker_logs(&job) else {
            continue;
        };
        indexed_jobs += 1;
        for (set, key) in [
            (&mut repos, "repo"),
            (&mut workflows, "workflow"),
            (&mut job_names, "job_name"),
            (&mut branches, "head_branch"),
        ] {
            if let Some(value) = job[key].as_str().filter(|value| !value.is_empty()) {
                set.insert(value.to_string());
            }
        }
        for line in parse_lines(&log) {
            indexed_lines += 1;
            if let Some(step) = line.step_name.filter(|step| !step.is_empty()) {
                steps.insert(step);
            }
        }
    }

    json!({
        "indexed_jobs": indexed_jobs,
        "indexed_lines": indexed_lines,
        "repos": values(repos),
        "workflows": values(workflows),
        "job_names": values(job_names),
        "branches": values(branches),
        "steps": values(steps),
        "levels": ["error", "warn", "info", "debug"],
    })
}

pub fn steps(job: &Value) -> Result<Value> {
    let log = agent::read_worker_logs(job)?;
    let mut positions: HashMap<String, usize> = HashMap::new();
    let mut steps: Vec<Value> = Vec::new();
    for line in parse_lines(&log) {
        let Some(name) = line.step_name else {
            continue;
        };
        let index = *positions.entry(name.clone()).or_insert_with(|| {
            steps.push(json!({
                "name": name,
                "first_line": line.line_no,
                "last_line": line.line_no,
                "lines": 0,
                "errors": 0,
                "warnings": 0,
            }));
            steps.len() - 1
        });
        steps[index]["last_line"] = json!(line.line_no);
        steps[index]["lines"] = json!(steps[index]["lines"].as_u64().unwrap_or(0) + 1);
        if line.level == "error" {
            steps[index]["errors"] = json!(steps[index]["errors"].as_u64().unwrap_or(0) + 1);
        } else if line.level == "warn" {
            steps[index]["warnings"] = json!(steps[index]["warnings"].as_u64().unwrap_or(0) + 1);
        }
    }
    Ok(json!({"job": job, "steps": steps}))
}

pub fn search_text(report: &Value) -> String {
    let mut out = format!(
        "{} matches across {} jobs{}\n",
        report["matches"].as_u64().unwrap_or(0),
        report["scanned_jobs"].as_u64().unwrap_or(0),
        if report["truncated"].as_bool() == Some(true) {
            " (output truncated)"
        } else {
            ""
        },
    );
    for item in report["results"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "{} {} / {} / {}{}:{} [{}]\n{}\n\n",
            item["gh_job_id"].as_i64().unwrap_or(0),
            item["repo"].as_str().unwrap_or(""),
            item["workflow"].as_str().unwrap_or(""),
            item["job_name"].as_str().unwrap_or(""),
            item["step_name"]
                .as_str()
                .map(|step| format!(" / {step}"))
                .unwrap_or_default(),
            item["line_no"].as_u64().unwrap_or(0),
            item["level"].as_str().unwrap_or("info"),
            item["message"].as_str().unwrap_or(""),
        ));
    }
    out
}

pub fn steps_text(report: &Value) -> String {
    let mut out = String::new();
    for step in report["steps"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "{:>6}-{:>6}  {:>6} lines  {:>3} errors  {:>3} warnings  {}\n",
            step["first_line"].as_u64().unwrap_or(0),
            step["last_line"].as_u64().unwrap_or(0),
            step["lines"].as_u64().unwrap_or(0),
            step["errors"].as_u64().unwrap_or(0),
            step["warnings"].as_u64().unwrap_or(0),
            step["name"].as_str().unwrap_or(""),
        ));
    }
    if out.is_empty() {
        out.push_str("no step boundaries detected\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::path::Path;

    #[test]
    fn query_parser_handles_phrases_exclusions_and_properties() {
        let parsed =
            parse_query(r#"timeout "connection refused" -retry level:error,warn"#).unwrap();
        assert_eq!(parsed.include, ["timeout", "connection refused"]);
        assert_eq!(parsed.exclude, ["retry"]);
        assert_eq!(parsed.properties.len(), 2);
        assert!(tokens("\"unterminated").is_err());
    }

    #[test]
    fn parser_tracks_step_boundaries_and_severity() {
        let lines = parse_lines(
            "Starting: Run tests\nordinary\n##[error] failed\nFinishing: Run tests\noutside",
        );
        assert_eq!(lines[0].step_name.as_deref(), Some("Run tests"));
        assert_eq!(lines[2].level, "error");
        assert_eq!(lines[4].step_name, None);
    }

    #[test]
    fn comma_separated_property_values_are_alternatives() {
        let parts = parse_query("level:error,warn").unwrap();
        let job = json!({});
        let line = ParsedLine {
            line_no: 1,
            step_name: None,
            level: "warn",
            message: "warning".into(),
        };
        assert!(properties_match(&job, Some(&line), &parts));
    }

    #[test]
    fn search_returns_matching_lines_with_job_and_step_context() {
        let dir = TempDir::new("global-log-search");
        let capture = dir.path().join("42");
        let diag = capture.join("diag");
        std::fs::create_dir_all(&diag).unwrap();
        std::fs::write(
            diag.join("Worker_001.log"),
            "Starting: Run tests\nconnection refused\nFinishing: Run tests\n",
        )
        .unwrap();
        let store = Store::open(Path::new(":memory:")).unwrap();
        let info = json!({
            "job_id": 42,
            "workflow": "CI",
            "job_name": "tests",
            "head_branch": "main",
        });
        store.job_started(&info, "owner/repo", "runner", Some(1.0));
        store.job_concluded(42, "failure");
        store.set_job_artifacts(42, Some(capture.to_str().unwrap()), None, None);

        let report = search(
            &store,
            &SearchQuery {
                q: Some("repo:owner/repo step:\"Run tests\" refused".into()),
                since: Some("all".into()),
                ..SearchQuery::default()
            },
        )
        .unwrap();
        assert_eq!(report["matches"], 1);
        assert_eq!(report["results"][0]["gh_job_id"], 42);
        assert_eq!(report["results"][0]["step_name"], "Run tests");

        let job = store.job(42).unwrap();
        let timeline = steps(&job).unwrap();
        assert_eq!(timeline["steps"][0]["name"], "Run tests");
        assert_eq!(timeline["steps"][0]["lines"], 3);
    }
}
