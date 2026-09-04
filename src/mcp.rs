//! Minimal MCP server over stdio (newline-delimited JSON-RPC). Exposes the
//! same queries as the CLI verbs so any MCP client — claude.ai, Claude Code,
//! anything — can ask about runner state and CI failures.
//!
//! Register with: `claude mcp add homerunner -- homerunner mcp`

use crate::agent;
use crate::config::Config;
use crate::log_search;
use crate::store::Store;
use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn tools() -> Value {
    json!({ "tools": [
        {
            "name": "runner_status",
            "description": "Live pools and runners: which repos are served, how many runners are listening or busy, and what jobs are running right now.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_jobs",
            "description": "Recent CI jobs run on this machine, newest first, with conclusions, durations, and whether captured logs / a kept post-mortem workspace exist.",
            "inputSchema": { "type": "object", "properties": {
                "limit": { "type": "integer", "description": "max jobs to return (default 20)" }
            } }
        },
        {
            "name": "analyze_ci",
            "description": "Cross-run CI analytics grouped by repository, workflow, and job: pass rate, duration percentiles, memory percentiles, OOMs, regressions, and tuning recommendations.",
            "inputSchema": { "type": "object", "properties": {
                "repo": { "type": "string", "description": "optional owner/repo filter" },
                "workflow": { "type": "string", "description": "optional exact workflow-name filter" },
                "job_name": { "type": "string", "description": "optional exact job-name filter" },
                "since": { "type": "string", "description": "lookback such as 24h, 7d, 4w, or all (default 30d)" }
            } }
        },
        {
            "name": "job_logs",
            "description": "Full captured step logs for one job (the runner's Worker diagnostics). job: numeric id, 'latest', or 'latest-failed' (default latest).",
            "inputSchema": { "type": "object", "properties": {
                "job": { "type": "string", "description": "job id, 'latest', or 'latest-failed'" }
            } }
        },
        {
            "name": "search_logs",
            "description": "Search all retained CI logs and return matching lines with repository, workflow, job, branch, step, severity, and line-number context. Query supports quoted phrases, exclusions, and repo:/workflow:/job_name:/branch:/step:/level: filters.",
            "inputSchema": { "type": "object", "properties": {
                "query": { "type": "string", "description": "text and/or property filters" },
                "since": { "type": "string", "description": "lookback such as 24h, 7d, 4w, or all (default 30d)" },
                "limit": { "type": "integer", "description": "maximum matching lines (default 100, max 1000)" }
            }, "required": ["query"] }
        },
        {
            "name": "job_steps",
            "description": "Detected workflow steps for a captured job, with line ranges and error/warning counts.",
            "inputSchema": { "type": "object", "properties": {
                "job": { "type": "string", "description": "job id, latest, or latest-failed" }
            } }
        },
        {
            "name": "why_failed",
            "description": "Failure digest for a job (default: most recent failed job): what ran, conclusion, log excerpt around the error, and whether a post-mortem workspace was kept.",
            "inputSchema": { "type": "object", "properties": {
                "job": { "type": "string", "description": "job id, 'latest', or 'latest-failed' (default latest-failed)" }
            } }
        }
    ] })
}

async fn call_tool(cfg: &Config, dashboard_port: u16, params: &Value) -> String {
    let name = params["name"].as_str().unwrap_or("");
    let args = &params["arguments"];
    let run = || -> Result<String> {
        let store = Store::open_readonly(&cfg.db_path())?;
        match name {
            "list_jobs" => {
                let limit = args["limit"].as_u64().unwrap_or(20) as u32;
                Ok(serde_json::to_string_pretty(&store.recent_jobs(limit))?)
            }
            "analyze_ci" => {
                let report = agent::analytics(
                    &store,
                    &agent::AnalyticsQuery {
                        repo: args["repo"].as_str().map(str::to_string),
                        workflow: args["workflow"].as_str().map(str::to_string),
                        job_name: args["job_name"].as_str().map(str::to_string),
                        since: args["since"].as_str().map(str::to_string),
                    },
                )?;
                Ok(serde_json::to_string_pretty(&report)?)
            }
            "job_logs" => {
                let job = agent::resolve_job(&store, args["job"].as_str(), false)?;
                let logs = agent::read_worker_logs(&job)?;
                // keep the payload sane for a model context window
                let lines: Vec<&str> = logs.lines().collect();
                let tail = lines[lines.len().saturating_sub(1200)..].join("\n");
                Ok(tail)
            }
            "search_logs" => {
                let report = log_search::search(
                    &store,
                    &log_search::SearchQuery {
                        q: args["query"].as_str().map(str::to_string),
                        since: args["since"].as_str().map(str::to_string),
                        limit: args["limit"].as_u64().map(|value| value as usize),
                        ..log_search::SearchQuery::default()
                    },
                )?;
                Ok(serde_json::to_string_pretty(&report)?)
            }
            "job_steps" => {
                let job = agent::resolve_job(&store, args["job"].as_str(), false)?;
                Ok(serde_json::to_string_pretty(&log_search::steps(&job)?)?)
            }
            "why_failed" => Ok(agent::why_text(&agent::why(&store, args["job"].as_str())?)),
            other => anyhow::bail!("unknown tool: {other}"),
        }
    };
    if name == "runner_status" {
        // Live state comes from the running supervisor; fall back gracefully.
        return match reqwest::Client::new()
            .get(format!("http://127.0.0.1:{dashboard_port}/api/state"))
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
        {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(mut v) => {
                    v.as_object_mut().map(|o| o.remove("events"));
                    serde_json::to_string_pretty(&v).unwrap_or_default()
                }
                Err(e) => format!("supervisor responded but state was unreadable: {e}"),
            },
            Err(_) => "supervisor is not running (dashboard unreachable) — job history is still available via list_jobs".into(),
        };
    }
    run().unwrap_or_else(|e| format!("error: {e}"))
}

pub async fn serve(cfg: Config) -> Result<()> {
    let port = cfg.dashboard_port;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = msg["method"].as_str().unwrap_or("");
        let id = msg["id"].clone();
        if id.is_null() {
            continue; // notification — nothing to answer
        }
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2024-11-05"),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "homerunner", "version": env!("CARGO_PKG_VERSION") }
                }
            }),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": tools() }),
            "tools/call" => {
                let text = call_tool(&cfg, port, &msg["params"]).await;
                let is_error = text.starts_with("error: ");
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "content": [{ "type": "text", "text": text }], "isError": is_error }
                })
            }
            _ => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") }
            }),
        };
        out.write_all(format!("{response}\n").as_bytes()).await?;
        out.flush().await?;
    }
    Ok(())
}
