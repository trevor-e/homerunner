"""SQLite journal. Never authoritative — the container runtime and the jobs
API are the sources of truth; this is history for the dashboard and post-crash
reconciliation."""

from __future__ import annotations

import sqlite3
import time
from pathlib import Path

SCHEMA = """
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
"""


class Store:
    def __init__(self, path: Path) -> None:
        self._db = sqlite3.connect(path)
        self._db.row_factory = sqlite3.Row
        self._db.execute("PRAGMA journal_mode=WAL")
        self._db.executescript(SCHEMA)

    def close(self) -> None:
        self._db.close()

    def upsert_runner(self, name: str, **fields) -> None:
        cols = ", ".join(fields)
        placeholders = ", ".join("?" for _ in fields)
        updates = ", ".join(f"{k}=excluded.{k}" for k in fields)
        self._db.execute(
            f"INSERT INTO runners (name, {cols}) VALUES (?, {placeholders}) "
            f"ON CONFLICT(name) DO UPDATE SET {updates}",
            (name, *fields.values()),
        )
        self._db.commit()

    def upsert_job(self, gh_job_id: int, **fields) -> None:
        cols = ", ".join(fields)
        placeholders = ", ".join("?" for _ in fields)
        updates = ", ".join(f"{k}=excluded.{k}" for k in fields)
        self._db.execute(
            f"INSERT INTO jobs (gh_job_id, {cols}) VALUES (?, {placeholders}) "
            f"ON CONFLICT(gh_job_id) DO UPDATE SET {updates}",
            (gh_job_id, *fields.values()),
        )
        self._db.commit()

    def event(self, level: str, source: str, msg: str) -> None:
        self._db.execute(
            "INSERT INTO events (ts, level, source, msg) VALUES (?, ?, ?, ?)",
            (time.time(), level, source, msg),
        )
        self._db.commit()

    def recent_jobs(self, limit: int = 50) -> list[dict]:
        rows = self._db.execute(
            "SELECT * FROM jobs ORDER BY COALESCE(completed_at, started_at) DESC LIMIT ?",
            (limit,),
        ).fetchall()
        return [dict(r) for r in rows]

    def recent_events(self, limit: int = 200) -> list[dict]:
        rows = self._db.execute(
            "SELECT * FROM events ORDER BY ts DESC LIMIT ?", (limit,)
        ).fetchall()
        return [dict(r) for r in rows]
