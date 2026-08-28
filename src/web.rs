//! Localhost dashboard: state snapshot + SSE change feed + runner log tails.
//! Full step logs live in GitHub's Actions UI; this covers the infra side.

use crate::scheduler::App;
use axum::extract::{Path, State};
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use futures::stream::Stream;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};
use tokio_stream::StreamExt;

const INDEX_HTML: &str = include_str!("../assets/index.html");

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/state", get(state))
        .route("/api/rate", get(rate))
        .route("/events", get(events))
        .route("/api/runners/{name}/logs", get(runner_logs))
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
            "retention": format!("newest {keep_logs} jobs kept"),
        },
        "journal": {
            "path": db_path.to_string_lossy(), "bytes": db_bytes,
            "retention": "events pruned after 7 days",
        },
        "images": {
            "list": images,
            "retention": format!("kept workspaces: newest {keep_ws} failed jobs"),
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
    let stream = BroadcastStream::new(rx).map(|_| Ok(Event::default().data("changed")));
    Sse::new(stream)
}

async fn runner_logs(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
) -> axum::response::Response {
    let target = {
        let runners = app.runners.lock().unwrap();
        runners
            .get(&name)
            .map(|r| (r.repo_cfg.runtime, r.container_id.clone()))
    };
    let Some((kind, container_id)) = target else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            format!("no live runner {name}"),
        )
            .into_response();
    };
    let Ok(rx) = kind.logs(&container_id) else {
        return (axum::http::StatusCode::BAD_GATEWAY, "log stream failed").into_response();
    };
    let stream = ReceiverStream::new(rx).map(|line| {
        Ok::<_, Infallible>(Event::default().data(serde_json::json!(line).to_string()))
    });
    Sse::new(stream).into_response()
}
