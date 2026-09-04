//! Localhost control center: fleet state, job history, storage, activity, and
//! searchable views over captured and live runner logs.

use crate::scheduler::App;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use futures::stream::Stream;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

const INDEX_HTML: &str = include_str!("../assets/index.html");
const LOGS_INDEX_HTML: &str = include_str!("../assets/logs_index.html");
const LOGS_HTML: &str = include_str!("../assets/logs.html");

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/logs", get(logs_index))
        .route("/jobs/{id}/logs", get(log_viewer))
        .route("/runners/{name}/logs", get(log_viewer))
        .route("/api/state", get(state))
        .route("/api/rate", get(rate))
        .route("/events", get(events))
        .route("/api/runners/{name}/logs", get(runner_logs))
        .route("/api/jobs/{id}", get(job))
        .route("/api/jobs/{id}/logs", get(job_logs))
        .route("/api/disk", get(disk))
        .with_state(app)
}

fn dir_stats(path: &std::path::Path) -> (u64, u64) {
    fn walk(path: &std::path::Path, bytes: &mut u64) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, bytes);
            } else if let Ok(meta) = entry.metadata() {
                *bytes += meta.len();
            }
        }
    }
    let mut bytes = 0u64;
    let count = std::fs::read_dir(path)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .count() as u64
        })
        .unwrap_or(0);
    walk(path, &mut bytes);
    (count, bytes)
}

async fn docker_lines(args: &[&str]) -> Vec<String> {
    let out = tokio::process::Command::new("docker")
        .args(args)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// What homerunner keeps on disk, where, and each item's retention rule.
async fn disk(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    let data_dir = app.config.data_dir.clone();
    let jobs_dir = data_dir.join("jobs");
    let db_path = app.config.db_path();
    let keep_logs = app.config.keep_job_logs;
    let keep_ws = app.config.keep_failed_workspaces;
    let log_retention = [
        Some(format!("newest {keep_logs} jobs")),
        app.config
            .job_logs_max_age_days
            .map(|days| format!("max {days} days")),
        app.config
            .job_logs_max_bytes
            .map(|bytes| format!("max {} MB", bytes / (1024 * 1024))),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("; ");
    let image_retention = [
        Some(format!("newest {keep_ws} failed jobs")),
        app.config
            .failed_workspaces_max_age_days
            .map(|days| format!("max {days} days")),
        app.config
            .failed_workspaces_max_bytes
            .map(|bytes| format!("max {} MB", bytes / (1024 * 1024))),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("; ");

    let (jobs_count, jobs_bytes) = tokio::task::spawn_blocking(move || dir_stats(&jobs_dir))
        .await
        .unwrap_or((0, 0));
    let db_bytes: u64 = ["", "-wal", "-shm"]
        .iter()
        .filter_map(|suffix| {
            std::fs::metadata(format!("{}{suffix}", db_path.display()))
                .ok()
                .map(|m| m.len())
        })
        .sum();

    let mut images = Vec::new();
    for line in docker_lines(&[
        "image",
        "ls",
        "--format",
        "{{.Repository}}:{{.Tag}} {{.Size}}",
    ])
    .await
    {
        if line.starts_with("homerunner-runner") || line.starts_with("homerunner-kept") {
            let mut parts = line.splitn(2, ' ');
            images.push(serde_json::json!({
                "tag": parts.next().unwrap_or(""),
                "size": parts.next().unwrap_or(""),
            }));
        }
    }
    // Volume sizes come from `docker system df -v` (best-effort text parse).
    let mut volumes = Vec::new();
    for line in docker_lines(&["system", "df", "-v"]).await {
        let is_cache_volume = crate::runtime::CACHE_VOLUMES
            .iter()
            .any(|(name, _)| line.starts_with(name));
        if is_cache_volume {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 2 {
                volumes.push(serde_json::json!({
                    "name": fields[0],
                    "size": fields[fields.len() - 1],
                }));
            }
        }
    }

    Json(serde_json::json!({
        "job_logs": {
            "path": app.config.data_dir.join("jobs").to_string_lossy(),
            "jobs": jobs_count, "bytes": jobs_bytes,
            "retention": log_retention,
        },
        "journal": {
            "path": db_path.to_string_lossy(), "bytes": db_bytes,
            "retention": format!(
                "events: {} days; artifact-free jobs: {} days",
                app.config.event_history_days, app.config.job_history_days
            ),
        },
        "images": {
            "list": images,
            "retention": image_retention,
        },
        "volumes": {
            "list": volumes,
            "retention": "dependency caches; grow with churn, `docker volume rm` resets",
        },
    }))
}

async fn job_logs(State(app): State<Arc<App>>, Path(id): Path<i64>) -> axum::response::Response {
    let job = app.store.lock().unwrap().job(id);
    let Some(job) = job else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            format!("no job {id} recorded"),
        )
            .into_response();
    };
    match crate::agent::read_worker_logs(&job) {
        Ok(text) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        Err(e) => (axum::http::StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn logs_index() -> Html<&'static str> {
    Html(LOGS_INDEX_HTML)
}

async fn log_viewer() -> Html<&'static str> {
    Html(LOGS_HTML)
}

async fn job(State(app): State<Arc<App>>, Path(id): Path<i64>) -> axum::response::Response {
    match app.store.lock().unwrap().job(id) {
        Some(job) => Json(job).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("no job {id} recorded")})),
        )
            .into_response(),
    }
}

async fn state(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    let mut snapshot = app.snapshot();
    {
        let store = app.store.lock().unwrap();
        snapshot["jobs"] = serde_json::Value::Array(store.recent_jobs(50));
        snapshot["events"] = serde_json::Value::Array(store.recent_events(50));
    }
    Json(snapshot)
}

async fn rate(State(app): State<Arc<App>>) -> impl IntoResponse {
    match app.github.rate_limit().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn events(State(app): State<Arc<App>>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = app.change_tx.subscribe();
    let stream = BroadcastStream::new(rx).map(|msg| {
        let payload = msg
            .map(|v| v.to_string())
            .unwrap_or_else(|_| r#"{"kind":"lagged"}"#.to_string());
        Ok(Event::default().data(payload))
    });
    Sse::new(stream)
}

async fn runner_logs(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> axum::response::Response {
    let last_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let target = {
        let runners = app.runners.lock().unwrap();
        runners.get(&name).map(|r| r.subscribe_logs(last_id))
    };
    let Some((replay, rx)) = target else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            format!("no live runner {name}"),
        )
            .into_response();
    };
    let event = |(id, line): (u64, String)| {
        Ok::<_, Infallible>(
            Event::default()
                .id(id.to_string())
                .data(serde_json::json!(line).to_string()),
        )
    };
    let initial = tokio_stream::iter(replay.into_iter().map(event));
    let live = BroadcastStream::new(rx).filter_map(move |message| message.ok().map(event));
    let stream = initial.chain(live);
    Sse::new(stream).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RepoConfig, RuntimeKind};
    use crate::github::GitHub;
    use crate::scheduler::{RunnerInfo, RunnerState};
    use crate::store::Store;
    use crate::test_support::TempDir;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use std::collections::VecDeque;
    use std::path::Path;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    fn repo() -> RepoConfig {
        RepoConfig {
            repo: "owner/project".into(),
            runtime: RuntimeKind::Docker,
            labels: vec!["self-hosted".into()],
            image: "runner:test".into(),
            reserved: 1,
            max: 3,
            job_timeout_min: 60,
            caffeinate: false,
            registry_mirror: None,
        }
    }

    fn test_app(data_dir: &Path) -> Arc<App> {
        App::new(
            Config {
                dashboard_port: 0,
                max_total_runners: 2,
                data_dir: data_dir.into(),
                keep_failed_workspaces: 1,
                failed_workspaces_max_age_days: None,
                failed_workspaces_max_bytes: None,
                keep_job_logs: 10,
                job_logs_max_age_days: None,
                job_logs_max_bytes: None,
                job_history_days: 365,
                event_history_days: 7,
                service_log_max_bytes: 10 * 1024 * 1024,
                service_log_backups: 3,
                poll_interval_s: 30,
                idle_decay_min: 10,
                auth_source: "env:HOMERUNNER_TEST_TOKEN".into(),
                python_version: "3.13".into(),
                node_version: "24".into(),
                repos: vec![repo()],
            },
            GitHub::new("env:HOMERUNNER_TEST_TOKEN"),
            Store::open(Path::new(":memory:")).unwrap(),
            None,
        )
    }

    #[test]
    fn directory_stats_count_top_level_jobs_and_nested_bytes() {
        let dir = TempDir::new("dir-stats");
        let first = dir.path().join("job-1");
        let nested = dir.path().join("job-2/diag");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(first.join("one.log"), b"123").unwrap();
        std::fs::write(nested.join("two.log"), b"4567").unwrap();

        assert_eq!(dir_stats(dir.path()), (2, 7));
    }

    #[test]
    fn directory_stats_are_empty_for_missing_path() {
        let dir = TempDir::new("dir-stats-missing");
        assert_eq!(dir_stats(&dir.path().join("missing")), (0, 0));
    }

    #[test]
    fn embedded_log_pages_include_duration_and_bounded_live_rendering() {
        assert!(LOGS_INDEX_HTML.contains("<th>Duration</th>"));
        assert!(LOGS_HTML.contains("MAX_LIVE_LINES = 20000"));
        assert!(LOGS_HTML.contains("queueLiveLine"));
    }

    #[tokio::test]
    async fn state_endpoint_exposes_effective_capacity() {
        let dir = TempDir::new("web-state");
        let response = router(test_app(dir.path()))
            .oneshot(Request::get("/api/state").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["capacity"], 2);
        assert_eq!(body["max_total_runners"], 2);
        assert_eq!(body["repos"][0]["max"], 3);
    }

    #[tokio::test]
    async fn live_log_endpoint_resumes_after_last_event_id() {
        let dir = TempDir::new("web-live-logs");
        let app = test_app(dir.path());
        let (log_tx, _) = broadcast::channel(16);
        app.runners.lock().unwrap().insert(
            "runner-1".into(),
            RunnerInfo {
                repo_cfg: repo(),
                state: RunnerState::Listening,
                container_id: "container-1".into(),
                gh_runner_id: Some(1),
                created_at: 1.0,
                busy_at: None,
                ran_job: false,
                decaying: false,
                job: serde_json::json!({}),
                log_tail: VecDeque::from([
                    (1, "first".into()),
                    (2, "second".into()),
                    (3, "third".into()),
                ]),
                next_log_seq: 3,
                log_tx,
                cpu_pct: 0.0,
                mem_bytes: 0,
                peak_mem_bytes: 0,
            },
        );
        let response = router(app)
            .oneshot(
                Request::get("/api/runners/runner-1/logs")
                    .header("last-event-id", "2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "text/event-stream"
        );

        let frame = response.into_body().frame().await.unwrap().unwrap();
        let chunk = std::str::from_utf8(frame.data_ref().unwrap()).unwrap();
        assert!(chunk.contains("id: 3"));
        assert!(chunk.contains("data: \"third\""));
        assert!(!chunk.contains("second"));
    }

    #[tokio::test]
    async fn missing_live_runner_returns_not_found() {
        let dir = TempDir::new("web-missing-runner");
        let response = router(test_app(dir.path()))
            .oneshot(
                Request::get("/api/runners/missing/logs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn captured_log_endpoint_returns_sorted_worker_logs() {
        let dir = TempDir::new("web-job-logs");
        let app = test_app(dir.path());
        let capture = dir.path().join("jobs/42");
        let diag = capture.join("diag");
        std::fs::create_dir_all(&diag).unwrap();
        std::fs::write(diag.join("Worker_002.log"), "second\n").unwrap();
        std::fs::write(diag.join("Worker_001.log"), "first\n").unwrap();
        {
            let store = app.store.lock().unwrap();
            store.job_started(
                &serde_json::json!({"job_id": 42, "job_name": "tests"}),
                "owner/project",
                "runner-1",
                Some(10.0),
            );
            store.set_job_artifacts(42, Some(capture.to_string_lossy().as_ref()), None, None);
        }

        let response = router(app)
            .oneshot(
                Request::get("/api/jobs/42/logs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"first\nsecond\n");
    }
}
