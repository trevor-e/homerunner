//! Declarative local monitors for long jobs and noteworthy completion states.

use crate::config::MonitorConfig;
use regex::Regex;
use serde_json::Value;

pub fn matches_scope(monitor: &MonitorConfig, repo: &str, job: &Value) -> bool {
    field_matches(monitor.repo.as_deref(), Some(repo))
        && field_matches(monitor.workflow.as_deref(), job["workflow"].as_str())
        && field_matches(monitor.job_name.as_deref(), job["job_name"].as_str())
        && field_matches(monitor.branch.as_deref(), job["head_branch"].as_str())
}

fn field_matches(expected: Option<&str>, actual: Option<&str>) -> bool {
    expected.is_none_or(|expected| actual == Some(expected))
}

pub fn completion_reasons(
    monitor: &MonitorConfig,
    conclusion: Option<&str>,
    oom: bool,
    log: Option<&str>,
    consecutive_failures: u32,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if monitor.oom && oom {
        reasons.push("runner was OOM-killed".into());
    }
    if let Some(pattern) = &monitor.log_pattern {
        // Config validation guarantees this compiles.
        if log.is_some_and(|text| Regex::new(pattern).unwrap().is_match(text)) {
            reasons.push(format!("log matched /{pattern}/"));
        }
    }
    if conclusion == Some("failure") {
        if let Some(threshold) = monitor.consecutive_failures {
            if consecutive_failures >= threshold {
                reasons.push(format!("{consecutive_failures} consecutive failures"));
            }
        }
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn monitor() -> MonitorConfig {
        MonitorConfig {
            name: "guardrail".into(),
            repo: Some("owner/repo".into()),
            workflow: Some("CI".into()),
            job_name: Some("tests".into()),
            branch: Some("main".into()),
            duration_min: None,
            consecutive_failures: Some(2),
            oom: true,
            log_pattern: Some("(?i)deadlock".into()),
            retain_workspace: true,
        }
    }

    #[test]
    fn exact_scope_requires_every_configured_field() {
        let job = json!({
            "workflow": "CI", "job_name": "tests", "head_branch": "main"
        });
        assert!(matches_scope(&monitor(), "owner/repo", &job));
        assert!(!matches_scope(&monitor(), "owner/other", &job));
    }

    #[test]
    fn completion_can_report_multiple_reasons() {
        let reasons = completion_reasons(
            &monitor(),
            Some("failure"),
            true,
            Some("Detected DEADLOCK in worker"),
            3,
        );
        assert_eq!(reasons.len(), 3);
        assert!(reasons.iter().any(|reason| reason.contains("OOM")));
        assert!(reasons.iter().any(|reason| reason.contains("deadlock")));
        assert!(reasons
            .iter()
            .any(|reason| reason.contains("3 consecutive")));
    }
}
