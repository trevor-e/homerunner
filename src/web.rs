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
        .with_state(app)
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

async fn events(
    State(app): State<Arc<App>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
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
        runners.get(&name).map(|r| (r.repo_cfg.runtime, r.container_id.clone()))
    };
    let Some((kind, container_id)) = target else {
        return (axum::http::StatusCode::NOT_FOUND, format!("no live runner {name}")).into_response();
    };
    let Ok(rx) = kind.logs(&container_id) else {
        return (axum::http::StatusCode::BAD_GATEWAY, "log stream failed").into_response();
    };
    let stream = ReceiverStream::new(rx)
        .map(|line| Ok::<_, Infallible>(Event::default().data(serde_json::json!(line).to_string())));
    Sse::new(stream).into_response()
}
