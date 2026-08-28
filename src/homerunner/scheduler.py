"""Warm-pool scheduler: keeps `pool_size` ephemeral JIT runners listening per
repo. Event-driven only — jobs reach runners over the runner's own long-poll
to GitHub; the supervisor reacts to local container exits and log lines, never
to REST polling."""

from __future__ import annotations

import asyncio
import contextlib
import re
import secrets
import time
from collections import deque
from dataclasses import dataclass, field
from enum import StrEnum
from typing import Callable

from .config import Config, RepoConfig
from .github import GitHub, GitHubError
from .runtime import Runtime, RuntimeUnavailable, get_runtime
from .store import Store

RUNNING_JOB_RE = re.compile(r"Running job: (?P<job>.+)$")
COMPLETED_RE = re.compile(r"completed with result: (?P<result>\w+)")
RESULT_MAP = {"Succeeded": "success", "Failed": "failure", "Canceled": "cancelled"}


class RunnerState(StrEnum):
    SPAWNING = "spawning"
    LISTENING = "listening"
    BUSY = "busy"
    EXITED = "exited"
    FAILED = "failed"


@dataclass
class Runner:
    name: str
    repo_cfg: RepoConfig
    state: RunnerState = RunnerState.SPAWNING
    container_id: str = ""
    gh_runner_id: int | None = None
    created_at: float = field(default_factory=time.time)
    busy_at: float | None = None
    ran_job: bool = False
    job_info: dict = field(default_factory=dict)
    log_tail: deque[str] = field(default_factory=lambda: deque(maxlen=40))
    tasks: list[asyncio.Task] = field(default_factory=list)


class Scheduler:
    def __init__(
        self,
        config: Config,
        github: GitHub,
        store: Store,
        on_change: Callable[[], None] = lambda: None,
    ) -> None:
        self.config = config
        self.github = github
        self.store = store
        self.on_change = on_change
        self.runners: dict[str, Runner] = {}
        self.degraded: dict[str, str] = {}  # runtime name -> reason
        self._backoff: dict[str, int] = {}  # repo -> consecutive no-job exits
        self._runtimes: dict[str, Runtime] = {}
        self._caffeinate: asyncio.subprocess.Process | None = None
        self._bg: list[asyncio.Task] = []

    # -- lifecycle ---------------------------------------------------------

    async def start(self) -> None:
        for repo_cfg in self.config.repos:
            self._runtimes.setdefault(repo_cfg.runtime, get_runtime(repo_cfg.runtime))
        await self._check_runtimes()
        await self._adopt_orphans()
        await self._sweep_registrations()
        for repo_cfg in self.config.repos:
            await self._top_up(repo_cfg)
        self._bg.append(asyncio.create_task(self._watchdog()))
        self._log("info", "scheduler", "started")

    async def stop(self) -> None:
        # Deliberately leaves runner containers alive: they finish (or keep
        # listening for) their single job, and the next supervisor run
        # re-adopts them. Use `homerunner down` semantics elsewhere if needed.
        for task in self._bg:
            task.cancel()
        for runner in self.runners.values():
            for task in runner.tasks:
                task.cancel()
        await self._set_caffeinate(False)

    # -- startup reconciliation -------------------------------------------

    async def _check_runtimes(self) -> None:
        for name, runtime in self._runtimes.items():
            reason = await runtime.available()
            if reason:
                self.degraded[name] = reason
                self._log("warn", "runtime", f"{name} degraded: {reason}")
            else:
                self.degraded.pop(name, None)

    async def _adopt_orphans(self) -> None:
        by_repo = {rc.repo: rc for rc in self.config.repos}
        for runtime in self._runtimes.values():
            if runtime.name in self.degraded:
                continue
            for mc in await runtime.list_managed():
                repo_cfg = by_repo.get(mc.repo)
                if mc.running and repo_cfg and repo_cfg.runtime == runtime.name:
                    runner = Runner(
                        name=mc.runner_name,
                        repo_cfg=repo_cfg,
                        state=RunnerState.LISTENING,
                        container_id=mc.container_id,
                    )
                    self.runners[runner.name] = runner
                    self._start_watchers(runner)
                    self._log("info", "recover", f"re-adopted running runner {mc.runner_name}")
                else:
                    await runtime.remove(mc.container_id)
                    self._log("info", "recover", f"reaped stale container {mc.runner_name}")

    async def _sweep_registrations(self) -> None:
        """Delete offline hr-* registrations that have no live container."""
        live = {r.name for r in self.runners.values()}
        for repo_cfg in self.config.repos:
            try:
                for reg in await self.github.list_runners(repo_cfg.repo):
                    if (
                        reg["name"].startswith(f"hr-{repo_cfg.slug}-")
                        and reg.get("status") == "offline"
                        and reg["name"] not in live
                    ):
                        await self.github.delete_runner(repo_cfg.repo, reg["id"])
                        self._log("info", "recover", f"deleted orphan registration {reg['name']}")
            except GitHubError as exc:
                self._log("warn", "recover", f"registration sweep failed for {repo_cfg.repo}: {exc}")

    # -- pool management ---------------------------------------------------

    def _live_count(self, repo: str | None = None) -> int:
        return sum(
            1
            for r in self.runners.values()
            if r.state in (RunnerState.SPAWNING, RunnerState.LISTENING, RunnerState.BUSY)
            and (repo is None or r.repo_cfg.repo == repo)
        )

    async def _top_up(self, repo_cfg: RepoConfig) -> None:
        if repo_cfg.runtime in self.degraded:
            return
        while (
            self._live_count(repo_cfg.repo) < repo_cfg.pool_size
            and self._live_count() < self.config.max_total_runners
        ):
            await self._spawn(repo_cfg)

    async def _spawn(self, repo_cfg: RepoConfig) -> None:
        name = f"hr-{repo_cfg.slug}-{secrets.token_hex(3)}"
        runner = Runner(name=name, repo_cfg=repo_cfg)
        self.runners[name] = runner
        try:
            runner.gh_runner_id, jit_config = await self.github.generate_jitconfig(
                repo_cfg.repo, name, repo_cfg.labels
            )
            runtime = self._runtimes[repo_cfg.runtime]
            runner.container_id = await runtime.spawn(
                runner_name=name,
                repo=repo_cfg.repo,
                image=repo_cfg.image,
                jit_config=jit_config,
                registry_mirror=repo_cfg.registry_mirror,
            )
        except (GitHubError, RuntimeUnavailable, KeyError) as exc:
            runner.state = RunnerState.FAILED
            self._backoff[repo_cfg.repo] = self._backoff.get(repo_cfg.repo, 0) + 1
            self._log("error", "spawn", f"{name} failed: {exc}")
            self._record_runner(runner)
            return
        runner.state = RunnerState.LISTENING
        self._start_watchers(runner)
        self._record_runner(runner)
        self._log("info", "spawn", f"{name} listening for {repo_cfg.repo}")
        self.on_change()

    def _start_watchers(self, runner: Runner) -> None:
        runner.tasks.append(asyncio.create_task(self._watch_exit(runner)))
        runner.tasks.append(asyncio.create_task(self._watch_logs(runner)))

    # -- per-runner watchers (local events only) ---------------------------

    async def _watch_exit(self, runner: Runner) -> None:
        runtime = self._runtimes[runner.repo_cfg.runtime]
        code = await runtime.wait(runner.container_id)
        runner.state = RunnerState.EXITED
        await runtime.remove(runner.container_id)
        self._record_runner(runner, ended_at=time.time(), exit_code=code)
        repo = runner.repo_cfg.repo
        if runner.ran_job:
            self._backoff[repo] = 0
            self._log("info", "reap", f"{runner.name} finished its job (exit {code})")
        else:
            self._backoff[repo] = self._backoff.get(repo, 0) + 1
            self._log("warn", "reap", f"{runner.name} exited without a job (exit {code})")
        for task in runner.tasks:
            if task is not asyncio.current_task():
                task.cancel()
        await self._update_caffeinate()
        self.on_change()
        delay = min(60, 2 ** self._backoff.get(repo, 0)) if self._backoff.get(repo) else 0
        if delay:
            await asyncio.sleep(delay)
        await self._top_up(runner.repo_cfg)

    async def _watch_logs(self, runner: Runner) -> None:
        runtime = self._runtimes[runner.repo_cfg.runtime]
        async for line in runtime.logs(runner.container_id):
            runner.log_tail.append(line)
            if match := RUNNING_JOB_RE.search(line):
                # The runner prints each diagnostic both bare and timestamped.
                if runner.state is RunnerState.BUSY:
                    continue
                runner.state = RunnerState.BUSY
                runner.busy_at = time.time()
                runner.ran_job = True
                runner.job_info = {"job_name": match["job"]}
                await self._update_caffeinate()
                asyncio.create_task(self._enrich_job(runner))
                self._log("info", "job", f"{runner.name} running: {match['job']}")
                self.on_change()
            elif match := COMPLETED_RE.search(line):
                if runner.job_info.get("conclusion"):
                    continue
                conclusion = RESULT_MAP.get(match["result"], match["result"].lower())
                runner.job_info["conclusion"] = conclusion
                if job_id := runner.job_info.get("job_id"):
                    self.store.upsert_job(
                        job_id, conclusion=conclusion, completed_at=time.time()
                    )
                self._log("info", "job", f"{runner.name} job result: {conclusion}")
                self.on_change()

    async def _enrich_job(self, runner: Runner) -> None:
        """A few REST lookups per busy transition, purely for the dashboard.
        Retries because the jobs API lags the runner's own log line in
        reporting runner_name."""
        info = None
        for _ in range(5):
            try:
                info = await self.github.find_job_by_runner(runner.repo_cfg.repo, runner.name)
            except GitHubError:
                return
            if info or runner.state is not RunnerState.BUSY:
                break
            await asyncio.sleep(8)
        if info:
            runner.job_info.update(info)
            self.store.upsert_job(
                info["job_id"],
                repo=runner.repo_cfg.repo,
                run_id=info["run_id"],
                workflow=info["workflow"],
                job_name=info["job_name"],
                runner_name=runner.name,
                html_url=info["html_url"],
                started_at=runner.busy_at,
            )
            self.on_change()

    async def _watchdog(self) -> None:
        """Local timers: wedged-job kill + daily runner-release staleness note."""
        last_release_check = 0.0
        while True:
            await asyncio.sleep(60)
            now = time.time()
            for runner in list(self.runners.values()):
                limit = runner.repo_cfg.job_timeout_min * 60
                if runner.state is RunnerState.BUSY and runner.busy_at and now - runner.busy_at > limit:
                    self._log("error", "watchdog", f"{runner.name} exceeded job timeout; killing")
                    await self._runtimes[runner.repo_cfg.runtime].kill(runner.container_id)
            if now - last_release_check > 86400:
                last_release_check = now
                with contextlib.suppress(GitHubError):
                    latest = await self.github.latest_runner_release()
                    self._log("info", "staleness", f"latest actions/runner release: v{latest}")

    # -- helpers -----------------------------------------------------------

    async def _update_caffeinate(self) -> None:
        wanted = any(
            r.state is RunnerState.BUSY and r.repo_cfg.caffeinate for r in self.runners.values()
        )
        await self._set_caffeinate(wanted)

    async def _set_caffeinate(self, wanted: bool) -> None:
        if wanted and self._caffeinate is None:
            self._caffeinate = await asyncio.create_subprocess_exec("caffeinate", "-is")
        elif not wanted and self._caffeinate is not None:
            self._caffeinate.terminate()
            self._caffeinate = None

    def _record_runner(self, runner: Runner, **extra) -> None:
        self.store.upsert_runner(
            runner.name,
            repo=runner.repo_cfg.repo,
            runtime=runner.repo_cfg.runtime,
            container_id=runner.container_id,
            gh_runner_id=runner.gh_runner_id,
            state=str(runner.state),
            created_at=runner.created_at,
            **extra,
        )

    def _log(self, level: str, source: str, msg: str) -> None:
        print(f"[{source}] {msg}", flush=True)
        self.store.event(level, source, msg)
        self.on_change()

    def snapshot(self) -> dict:
        return {
            "degraded": self.degraded,
            "repos": [
                {
                    "repo": rc.repo,
                    "runtime": rc.runtime,
                    "pool_size": rc.pool_size,
                    "live": self._live_count(rc.repo),
                }
                for rc in self.config.repos
            ],
            "runners": [
                {
                    "name": r.name,
                    "repo": r.repo_cfg.repo,
                    "state": str(r.state),
                    "created_at": r.created_at,
                    "busy_at": r.busy_at,
                    "job": r.job_info,
                    "log_tail": list(r.log_tail)[-8:],
                }
                for r in self.runners.values()
                if r.state not in (RunnerState.EXITED, RunnerState.FAILED)
            ],
        }
