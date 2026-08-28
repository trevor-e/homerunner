//! SQLite journal. Never authoritative — the container runtime and the jobs
//! API are the sources of truth; this is history for the dashboard and
//! post-crash reconciliation.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runners (
  name TEXT PRIMARY KEY,
  repo TEXT NOT NULL,
  runtime TEXT NOT NULL,
  container_id TEXT,
  gh_runner_id INTEGER,
  state TEXT NOT NULL,
  created_at REAL NOT NULL,
  ended_at REAL,
  exit_code INTEGER
);
CREATE TABLE IF NOT EXISTS jobs (
  gh_job_id INTEGER PRIMARY KEY,
  repo TEXT NOT NULL,
  run_id INTEGER,
  workflow TEXT,
  job_name TEXT,
  conclusion TEXT,
  runner_name TEXT,
  html_url TEXT,
  started_at REAL,
  completed_at REAL
);
CREATE TABLE IF NOT EXISTS events (
  ts REAL NOT NULL,
  level TEXT NOT NULL,
  source TEXT NOT NULL,
  msg TEXT NOT NULL
);
";

pub struct Store {
    db: Connection,
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_runner(
        &self,
        name: &str,
        repo: &str,
        runtime: &str,
        container_id: &str,
        gh_runner_id: Option<i64>,
        state: &str,
        created_at: f64,
        ended_at: Option<f64>,
        exit_code: Option<i64>,
    ) {
        let _ = self.db.execute(
            "INSERT INTO runners (name, repo, runtime, container_id, gh_runner_id, state, created_at, ended_at, exit_code)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(name) DO UPDATE SET repo=?2, runtime=?3, container_id=?4, gh_runner_id=?5,
               state=?6, created_at=?7, ended_at=?8, exit_code=?9",
            params![name, repo, runtime, container_id, gh_runner_id, state, created_at, ended_at, exit_code],
        );
    }

    pub fn job_started(&self, info: &Value, repo: &str, runner_name: &str, started_at: Option<f64>) {
        let _ = self.db.execute(
            "INSERT INTO jobs (gh_job_id, repo, run_id, workflow, job_name, runner_name, html_url, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(gh_job_id) DO UPDATE SET run_id=?3, workflow=?4, job_name=?5, runner_name=?6,
               html_url=?7, started_at=?8",
            params![
                info["job_id"].as_i64(),
                repo,
                info["run_id"].as_i64(),
                info["workflow"].as_str(),
                info["job_name"].as_str(),
                runner_name,
                info["html_url"].as_str(),
                started_at,
            ],
        );
    }

    pub fn job_concluded(&self, gh_job_id: i64, conclusion: &str) {
        let _ = self.db.execute(
            "UPDATE jobs SET conclusion=?2, completed_at=?3 WHERE gh_job_id=?1",
            params![gh_job_id, conclusion, now()],
        );
    }

    pub fn event(&self, level: &str, source: &str, msg: &str) {
        let _ = self.db.execute(
            "INSERT INTO events (ts, level, source, msg) VALUES (?1, ?2, ?3, ?4)",
            params![now(), level, source, msg],
        );
    }

    pub fn recent_jobs(&self, limit: u32) -> Vec<Value> {
        self.rows(
            "SELECT gh_job_id, repo, run_id, workflow, job_name, conclusion, runner_name,
                    html_url, started_at, completed_at
             FROM jobs ORDER BY COALESCE(completed_at, started_at) DESC LIMIT ?1",
            limit,
            |row| {
                json!({
                    "gh_job_id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "repo": row.get::<_, Option<String>>(1).ok().flatten(),
                    "run_id": row.get::<_, Option<i64>>(2).ok().flatten(),
                    "workflow": row.get::<_, Option<String>>(3).ok().flatten(),
                    "job_name": row.get::<_, Option<String>>(4).ok().flatten(),
                    "conclusion": row.get::<_, Option<String>>(5).ok().flatten(),
                    "runner_name": row.get::<_, Option<String>>(6).ok().flatten(),
                    "html_url": row.get::<_, Option<String>>(7).ok().flatten(),
                    "started_at": row.get::<_, Option<f64>>(8).ok().flatten(),
                    "completed_at": row.get::<_, Option<f64>>(9).ok().flatten(),
                })
            },
        )
    }

    pub fn recent_events(&self, limit: u32) -> Vec<Value> {
        self.rows(
            "SELECT ts, level, source, msg FROM events ORDER BY ts DESC LIMIT ?1",
            limit,
            |row| {
                json!({
                    "ts": row.get::<_, Option<f64>>(0).ok().flatten(),
                    "level": row.get::<_, Option<String>>(1).ok().flatten(),
                    "source": row.get::<_, Option<String>>(2).ok().flatten(),
                    "msg": row.get::<_, Option<String>>(3).ok().flatten(),
                })
            },
        )
    }

    fn rows(
        &self,
        sql: &str,
        limit: u32,
        map: impl Fn(&rusqlite::Row<'_>) -> Value,
    ) -> Vec<Value> {
        let Ok(mut stmt) = self.db.prepare(sql) else {
            return Vec::new();
        };
        let Ok(mapped) = stmt.query_map([limit], |row| Ok(map(row))) else {
            return Vec::new();
        };
        mapped.filter_map(|r| r.ok()).collect()
    }
}
