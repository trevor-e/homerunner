"""Config loading: TOML -> dataclasses, with [defaults] merged into each repo."""

from __future__ import annotations

import tomllib
from dataclasses import dataclass, field
from pathlib import Path

REPO_DEFAULTS = {
    "runtime": "docker",
    "labels": ["self-hosted", "linux", "x64"],
    "image": "homerunner-runner:local",
    "pool_size": 1,
    "job_timeout_min": 120,
    "caffeinate": True,
    "registry_mirror": None,
}


@dataclass(frozen=True)
class RepoConfig:
    repo: str  # "owner/name"
    runtime: str
    labels: list[str]
    image: str
    pool_size: int
    job_timeout_min: int
    caffeinate: bool
    registry_mirror: str | None

    @property
    def slug(self) -> str:
        return self.repo.replace("/", "-")


@dataclass(frozen=True)
class Config:
    dashboard_port: int
    max_total_runners: int
    data_dir: Path
    auth_source: str
    repos: list[RepoConfig] = field(default_factory=list)

    @property
    def db_path(self) -> Path:
        return self.data_dir / "homerunner.db"


class ConfigError(Exception):
    pass


def load(path: Path) -> Config:
    if not path.exists():
        raise ConfigError(f"config file not found: {path}")
    with path.open("rb") as f:
        raw = tomllib.load(f)

    sup = raw.get("supervisor", {})
    defaults = {**REPO_DEFAULTS, **raw.get("defaults", {})}

    repos: list[RepoConfig] = []
    for entry in raw.get("repos", []):
        merged = {**defaults, **entry}
        repo = merged.pop("repo", None)
        if not repo or "/" not in repo:
            raise ConfigError(f"[[repos]] entry needs repo = 'owner/name', got: {entry}")
        unknown = set(merged) - set(REPO_DEFAULTS)
        if unknown:
            raise ConfigError(f"unknown keys for repo {repo}: {sorted(unknown)}")
        if merged["runtime"] not in ("docker", "apple-container"):
            raise ConfigError(f"repo {repo}: runtime must be 'docker' or 'apple-container'")
        repos.append(RepoConfig(repo=repo, **merged))

    if not repos:
        raise ConfigError("no [[repos]] configured")

    data_dir = Path(sup.get("data_dir", "~/.local/share/homerunner")).expanduser()
    data_dir.mkdir(parents=True, exist_ok=True)

    return Config(
        dashboard_port=int(sup.get("dashboard_port", 8123)),
        max_total_runners=int(sup.get("max_total_runners", 4)),
        data_dir=data_dir,
        auth_source=raw.get("auth", {}).get("source", "gh"),
        repos=repos,
    )
