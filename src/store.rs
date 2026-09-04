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

#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactRecord {
    pub job_id: i64,
    pub completed_at: Option<f64>,
    pub log_dir: Option<String>,
    pub kept_image: Option<String>,
    pub kept_image_runtime: Option<String>,
}

#[derive(Debug, Default)]
pub struct JobFilter<'a> {
    pub repo: Option<&'a str>,
    pub workflow: Option<&'a str>,
    pub job_name: Option<&'a str>,
    pub completed_after: Option<f64>,
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
        // Additive migrations; errors mean the column already exists.
        for stmt in [
            "ALTER TABLE jobs ADD COLUMN log_dir TEXT",
            "ALTER TABLE jobs ADD COLUMN kept_image TEXT",
            "ALTER TABLE jobs ADD COLUMN kept_image_runtime TEXT",
            "ALTER TABLE jobs ADD COLUMN head_branch TEXT",
            "ALTER TABLE jobs ADD COLUMN head_sha TEXT",
            "ALTER TABLE jobs ADD COLUMN title TEXT",
            "ALTER TABLE jobs ADD COLUMN event TEXT",
            "ALTER TABLE jobs ADD COLUMN peak_mem_mb REAL",
            "ALTER TABLE jobs ADD COLUMN oom INTEGER",
            "ALTER TABLE jobs ADD COLUMN cpu_avg_pct REAL",
            "ALTER TABLE jobs ADD COLUMN cpu_peak_pct REAL",
        ] {
            let _ = db.execute(stmt, []);
        }
        // Workspace commits predate runtime tracking and were Docker-only.
        db.execute(
            "UPDATE jobs SET kept_image_runtime='docker'
             WHERE kept_image IS NOT NULL AND kept_image_runtime IS NULL",
            [],
        )?;
        Ok(Self { db })
    }

    /// Open for CLI commands: writes are refused via query_only, but the
    /// connection is a normal one — a strictly read-only connection can't
    /// apply the supervisor's WAL and silently reads stale (even empty) data.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        anyhow::ensure!(
            path.exists(),
            "no journal at {} (has the supervisor run?)",
            path.display()
        );
        let db = Connection::open(path)?;
        db.pragma_update(None, "query_only", "ON")?;
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

    pub fn job_started(
        &self,
        info: &Value,
        repo: &str,
        runner_name: &str,
        started_at: Option<f64>,
    ) {
        let _ = self.db.execute(
            "INSERT INTO jobs (gh_job_id, repo, run_id, workflow, job_name, runner_name, html_url,
                               started_at, head_branch, head_sha, title, event)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(gh_job_id) DO UPDATE SET run_id=?3, workflow=?4, job_name=?5, runner_name=?6,
               html_url=?7, started_at=?8, head_branch=?9, head_sha=?10, title=?11, event=?12",
            params![
                info["job_id"].as_i64(),
                repo,
                info["run_id"].as_i64(),
                info["workflow"].as_str(),
                info["job_name"].as_str(),
                runner_name,
                info["html_url"].as_str(),
                started_at,
                info["head_branch"].as_str(),
                info["head_sha"].as_str(),
                info["title"].as_str(),
                info["event"].as_str(),
            ],
        );
    }

    pub fn job_concluded(&self, gh_job_id: i64, conclusion: &str) {
        let _ = self.db.execute(
            "UPDATE jobs SET conclusion=?2, completed_at=?3 WHERE gh_job_id=?1",
            params![gh_job_id, conclusion, now()],
        );
    }

    pub fn set_job_artifacts(
        &self,
        gh_job_id: i64,
        log_dir: Option<&str>,
        kept_image: Option<&str>,
        kept_image_runtime: Option<&str>,
    ) {
        let _ = self.db.execute(
            "UPDATE jobs SET log_dir=COALESCE(?2, log_dir), kept_image=COALESCE(?3, kept_image),
             kept_image_runtime=COALESCE(?4, kept_image_runtime)
             WHERE gh_job_id=?1",
            params![gh_job_id, log_dir, kept_image, kept_image_runtime],
        );
    }

    pub fn set_job_resources(
        &self,
        gh_job_id: i64,
        peak_mem_mb: f64,
        oom: bool,
        cpu_avg_pct: Option<f64>,
        cpu_peak_pct: Option<f64>,
    ) {
        let _ = self.db.execute(
            "UPDATE jobs SET peak_mem_mb=?2, oom=?3, cpu_avg_pct=?4, cpu_peak_pct=?5
             WHERE gh_job_id=?1",
            params![
                gh_job_id,
                peak_mem_mb,
                oom as i64,
                cpu_avg_pct,
                cpu_peak_pct
            ],
        );
    }

    pub fn clear_log_dir(&self, log_dir: &str) -> Result<usize> {
        Ok(self.db.execute(
            "UPDATE jobs SET log_dir=NULL WHERE log_dir=?1",
            params![log_dir],
        )?)
    }

    pub fn prune_events(&self, older_than_s: f64) -> Result<usize> {
        Ok(self.db.execute(
            "DELETE FROM events WHERE ts < ?1",
            params![now() - older_than_s],
        )?)
    }

    pub fn count_prunable_events(&self, older_than_s: f64) -> usize {
        self.db
            .query_row(
                "SELECT COUNT(*) FROM events WHERE ts < ?1",
                [now() - older_than_s],
                |row| row.get(0),
            )
            .unwrap_or(0)
    }

    pub fn clear_kept_image(&self, gh_job_id: i64) -> Result<usize> {
        Ok(self.db.execute(
            "UPDATE jobs SET kept_image=NULL, kept_image_runtime=NULL WHERE gh_job_id=?1",
            params![gh_job_id],
        )?)
    }

    pub fn artifact_records(&self) -> Vec<ArtifactRecord> {
        self.artifact_records_query(
            "SELECT gh_job_id, completed_at, log_dir, kept_image, kept_image_runtime
             FROM jobs ORDER BY COALESCE(completed_at, started_at) ASC",
        )
        .or_else(|| {
            self.artifact_records_query(
                "SELECT gh_job_id, completed_at, log_dir, kept_image, NULL
                 FROM jobs ORDER BY COALESCE(completed_at, started_at) ASC",
            )
        })
        .or_else(|| {
            self.artifact_records_query(
                "SELECT gh_job_id, completed_at, NULL, NULL, NULL
                 FROM jobs ORDER BY COALESCE(completed_at, started_at) ASC",
            )
        })
        .unwrap_or_default()
    }

    fn artifact_records_query(&self, sql: &str) -> Option<Vec<ArtifactRecord>> {
        let mut stmt = self.db.prepare(sql).ok()?;
        let Ok(rows) = stmt.query_map([], |r| {
            Ok(ArtifactRecord {
                job_id: r.get(0)?,
                completed_at: r.get(1)?,
                log_dir: r.get(2)?,
                kept_image: r.get(3)?,
                kept_image_runtime: r.get(4)?,
            })
        }) else {
            return None;
        };
        Some(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn repair_log_dir(&self, gh_job_id: i64, log_dir: &str) -> Result<usize> {
        Ok(self.db.execute(
            "UPDATE jobs SET log_dir=?2 WHERE gh_job_id=?1 AND log_dir IS NULL",
            params![gh_job_id, log_dir],
        )?)
    }

    pub fn repair_kept_image(&self, gh_job_id: i64, tag: &str, runtime: &str) -> Result<usize> {
        Ok(self.db.execute(
            "UPDATE jobs SET kept_image=?2, kept_image_runtime=?3
             WHERE gh_job_id=?1 AND kept_image IS NULL",
            params![gh_job_id, tag, runtime],
        )?)
    }

    pub fn count_prunable_jobs(&self, older_than_s: f64) -> usize {
        self.db
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE completed_at < ?1
                 AND log_dir IS NULL AND kept_image IS NULL",
                [now() - older_than_s],
                |row| row.get(0),
            )
            .unwrap_or(0)
    }

    pub fn prune_jobs(&self, older_than_s: f64) -> Result<usize> {
        Ok(self.db.execute(
            "DELETE FROM jobs WHERE completed_at < ?1
             AND log_dir IS NULL AND kept_image IS NULL",
            [now() - older_than_s],
        )?)
    }

    pub fn job(&self, gh_job_id: i64) -> Option<Value> {
        self.job_where("gh_job_id = ?1", params![gh_job_id])
    }

    /// Most recent job; `failed_only` restricts to conclusion = failure.
    pub fn latest_job(&self, failed_only: bool) -> Option<Value> {
        if failed_only {
            self.job_where("conclusion = 'failure'", params![])
        } else {
            self.job_where("1=1", params![])
        }
    }

    fn job_where(&self, cond: &str, args: impl rusqlite::Params) -> Option<Value> {
        let sql = format!(
            "SELECT gh_job_id, repo, run_id, workflow, job_name, conclusion, runner_name,
                    html_url, started_at, completed_at, log_dir, kept_image,
                    head_branch, head_sha, title, event, peak_mem_mb, oom,
                    cpu_avg_pct, cpu_peak_pct
             FROM jobs WHERE {cond} ORDER BY COALESCE(completed_at, started_at) DESC LIMIT 1"
        );
        let mut stmt = self.db.prepare(&sql).ok()?;
        stmt.query_row(args, |row| Ok(Self::job_row(row))).ok()
    }

    fn job_row(row: &rusqlite::Row<'_>) -> Value {
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
            "log_dir": row.get::<_, Option<String>>(10).ok().flatten(),
            "kept_image": row.get::<_, Option<String>>(11).ok().flatten(),
            "head_branch": row.get::<_, Option<String>>(12).ok().flatten(),
            "head_sha": row.get::<_, Option<String>>(13).ok().flatten(),
            "title": row.get::<_, Option<String>>(14).ok().flatten(),
            "event": row.get::<_, Option<String>>(15).ok().flatten(),
            "peak_mem_mb": row.get::<_, Option<f64>>(16).ok().flatten(),
            "oom": row.get::<_, Option<i64>>(17).ok().flatten().map(|v| v != 0),
            "cpu_avg_pct": row.get::<_, Option<f64>>(18).ok().flatten(),
            "cpu_peak_pct": row.get::<_, Option<f64>>(19).ok().flatten(),
        })
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
                    html_url, started_at, completed_at, log_dir, kept_image,
                    head_branch, head_sha, title, event, peak_mem_mb, oom,
                    cpu_avg_pct, cpu_peak_pct
             FROM jobs ORDER BY COALESCE(completed_at, started_at) DESC LIMIT ?1",
            limit,
            Self::job_row,
        )
    }

    /// Completed jobs matching the analytics dimensions, newest first.
    pub fn filtered_jobs(&self, filter: &JobFilter<'_>) -> Vec<Value> {
        let Ok(mut stmt) = self.db.prepare(
            "SELECT gh_job_id, repo, run_id, workflow, job_name, conclusion, runner_name,
                    html_url, started_at, completed_at, log_dir, kept_image,
                    head_branch, head_sha, title, event, peak_mem_mb, oom,
                    cpu_avg_pct, cpu_peak_pct
             FROM jobs
             WHERE conclusion IS NOT NULL
               AND (?1 IS NULL OR repo = ?1)
               AND (?2 IS NULL OR workflow = ?2)
               AND (?3 IS NULL OR job_name = ?3)
               AND (?4 IS NULL OR completed_at >= ?4)
             ORDER BY completed_at DESC",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map(
            params![
                filter.repo,
                filter.workflow,
                filter.job_name,
                filter.completed_after,
            ],
            |row| Ok(Self::job_row(row)),
        ) else {
            return Vec::new();
        };
        rows.filter_map(|row| row.ok()).collect()
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

    fn rows(&self, sql: &str, limit: u32, map: impl Fn(&rusqlite::Row<'_>) -> Value) -> Vec<Value> {
        let Ok(mut stmt) = self.db.prepare(sql) else {
            return Vec::new();
        };
        let Ok(mapped) = stmt.query_map([limit], |row| Ok(map(row))) else {
            return Vec::new();
        };
        mapped.filter_map(|r| r.ok()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn memory_store() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }

    fn job_info(id: i64, name: &str) -> Value {
        json!({
            "job_id": id,
            "run_id": id * 10,
            "workflow": "CI",
            "job_name": name,
            "html_url": format!("https://example.test/jobs/{id}"),
            "head_branch": "main",
            "head_sha": format!("sha-{id}"),
            "title": format!("Change {id}"),
            "event": "push",
        })
    }

    #[test]
    fn job_lifecycle_round_trips_dashboard_fields() {
        let store = memory_store();
        store.job_started(&job_info(42, "tests"), "owner/repo", "runner-1", Some(10.0));
        store.set_job_artifacts(42, Some("/logs/42"), Some("kept:42"), Some("docker"));
        store.set_job_resources(42, 384.0, true, Some(72.0), Some(130.0));
        store.job_concluded(42, "failure");

        let job = store.job(42).unwrap();
        assert_eq!(job["repo"], "owner/repo");
        assert_eq!(job["workflow"], "CI");
        assert_eq!(job["job_name"], "tests");
        assert_eq!(job["runner_name"], "runner-1");
        assert_eq!(job["started_at"], 10.0);
        assert!(job["completed_at"].as_f64().unwrap() >= 10.0);
        assert_eq!(job["conclusion"], "failure");
        assert_eq!(job["log_dir"], "/logs/42");
        assert_eq!(job["kept_image"], "kept:42");
        assert_eq!(
            store.artifact_records()[0].kept_image_runtime.as_deref(),
            Some("docker")
        );
        assert_eq!(job["peak_mem_mb"], 384.0);
        assert_eq!(job["oom"], true);
        assert_eq!(job["cpu_avg_pct"], 72.0);
        assert_eq!(job["cpu_peak_pct"], 130.0);
        assert_eq!(job["head_branch"], "main");
        assert_eq!(job["head_sha"], "sha-42");
        assert_eq!(job["title"], "Change 42");
        assert_eq!(job["event"], "push");
    }

    #[test]
    fn recent_jobs_are_newest_first_and_honor_limit() {
        let store = memory_store();
        store.job_started(&job_info(1, "old"), "owner/repo", "runner-1", Some(10.0));
        store.job_started(&job_info(2, "new"), "owner/repo", "runner-2", Some(20.0));

        let jobs = store.recent_jobs(1);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["gh_job_id"], 2);
    }

    #[test]
    fn filtered_jobs_apply_dimensions_and_time_window() {
        let store = memory_store();
        store.job_started(&job_info(1, "tests"), "owner/one", "runner-1", Some(1.0));
        store.job_concluded(1, "success");
        store.job_started(&job_info(2, "lint"), "owner/two", "runner-2", Some(2.0));
        store.job_concluded(2, "failure");
        store
            .db
            .execute("UPDATE jobs SET completed_at=10 WHERE gh_job_id=1", [])
            .unwrap();
        store
            .db
            .execute("UPDATE jobs SET completed_at=20 WHERE gh_job_id=2", [])
            .unwrap();

        let jobs = store.filtered_jobs(&JobFilter {
            repo: Some("owner/two"),
            workflow: Some("CI"),
            job_name: Some("lint"),
            completed_after: Some(15.0),
        });
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["gh_job_id"], 2);
        assert!(store
            .filtered_jobs(&JobFilter {
                completed_after: Some(21.0),
                ..JobFilter::default()
            })
            .is_empty());
    }

    #[test]
    fn latest_failed_ignores_newer_successful_jobs() {
        let store = memory_store();
        store.job_started(&job_info(1, "failed"), "owner/repo", "runner-1", Some(10.0));
        store.job_concluded(1, "failure");
        store.job_started(&job_info(2, "passed"), "owner/repo", "runner-2", Some(20.0));
        store.job_concluded(2, "success");
        store
            .db
            .execute("UPDATE jobs SET completed_at=10 WHERE gh_job_id=1", [])
            .unwrap();
        store
            .db
            .execute("UPDATE jobs SET completed_at=20 WHERE gh_job_id=2", [])
            .unwrap();

        assert_eq!(store.latest_job(false).unwrap()["gh_job_id"], 2);
        assert_eq!(store.latest_job(true).unwrap()["gh_job_id"], 1);
    }

    #[test]
    fn artifact_references_can_be_cleared_after_gc() {
        let store = memory_store();
        store.job_started(&job_info(7, "failed"), "owner/repo", "runner", Some(1.0));
        store.set_job_artifacts(7, Some("/logs/7"), Some("kept:7"), Some("docker"));

        assert_eq!(
            store.artifact_records()[0].kept_image.as_deref(),
            Some("kept:7")
        );
        store.clear_log_dir("/logs/7").unwrap();
        store.clear_kept_image(7).unwrap();

        let job = store.job(7).unwrap();
        assert!(job["log_dir"].is_null());
        assert!(job["kept_image"].is_null());
        assert!(store.artifact_records()[0].kept_image.is_none());
    }

    #[test]
    fn event_pruning_removes_only_expired_rows() {
        let store = memory_store();
        store
            .db
            .execute(
                "INSERT INTO events (ts, level, source, msg) VALUES (0, 'info', 'old', 'expired')",
                [],
            )
            .unwrap();
        store.event("warn", "new", "current");

        store.prune_events(60.0).unwrap();
        let events = store.recent_events(10);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["source"], "new");
        assert_eq!(events[0]["msg"], "current");
    }

    #[test]
    fn job_pruning_preserves_rows_with_retained_artifacts() {
        let store = memory_store();
        store.job_started(&job_info(1, "plain"), "owner/repo", "runner-1", Some(1.0));
        store.job_started(&job_info(2, "logs"), "owner/repo", "runner-2", Some(1.0));
        store.set_job_artifacts(2, Some("/logs/2"), None, None);
        store
            .db
            .execute("UPDATE jobs SET completed_at=1", [])
            .unwrap();

        assert_eq!(store.count_prunable_jobs(1.0), 1);
        assert_eq!(store.prune_jobs(1.0).unwrap(), 1);
        assert!(store.job(1).is_none());
        assert!(store.job(2).is_some());
    }

    #[test]
    fn opening_an_old_database_applies_additive_migrations() {
        let dir = TempDir::new("store-migrations");
        let path = dir.path().join("journal.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db.execute_batch(
            "ALTER TABLE jobs ADD COLUMN log_dir TEXT;
             ALTER TABLE jobs ADD COLUMN kept_image TEXT;
             INSERT INTO jobs (gh_job_id, repo, kept_image)
             VALUES (99, 'owner/repo', 'homerunner-kept:99');",
        )
        .unwrap();
        drop(db);

        let store = Store::open(&path).unwrap();
        let mut statement = store.db.prepare("PRAGMA table_info(jobs)").unwrap();
        let columns: Vec<String> = statement
            .query_map([], |row| row.get(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for expected in [
            "log_dir",
            "kept_image",
            "kept_image_runtime",
            "head_branch",
            "head_sha",
            "title",
            "event",
            "peak_mem_mb",
            "oom",
            "cpu_avg_pct",
            "cpu_peak_pct",
        ] {
            assert!(columns.iter().any(|column| column == expected));
        }
        drop(statement);
        assert_eq!(
            store.artifact_records()[0].kept_image_runtime.as_deref(),
            Some("docker")
        );
    }

    #[test]
    fn readonly_store_rejects_writes() {
        let dir = TempDir::new("store-readonly");
        let path = dir.path().join("journal.db");
        drop(Store::open(&path).unwrap());
        let store = Store::open_readonly(&path).unwrap();

        assert!(store
            .db
            .execute(
                "INSERT INTO events (ts, level, source, msg) VALUES (0, 'info', 'test', 'no')",
                [],
            )
            .is_err());
    }
}
