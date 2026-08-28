"""Async GitHub REST client. Every call here is event-driven (spawn, busy
transition, startup sweep, daily staleness check) — the supervisor never polls
for work; runners hear about jobs over their own long-poll to GitHub's broker."""

from __future__ import annotations

import asyncio
import os
from pathlib import Path
from typing import Any

import aiohttp

API = "https://api.github.com"


class GitHubError(Exception):
    pass


async def resolve_token(source: str) -> str:
    if source == "gh":
        proc = await asyncio.create_subprocess_exec(
            "gh", "auth", "token",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        )
        out, err = await proc.communicate()
        if proc.returncode != 0:
            raise GitHubError(f"`gh auth token` failed: {err.decode().strip()}")
        return out.decode().strip()
    if source.startswith("env:"):
        token = os.environ.get(source[4:], "")
        if not token:
            raise GitHubError(f"env var {source[4:]} is empty")
        return token
    if source.startswith("file:"):
        return Path(source[5:]).expanduser().read_text().strip()
    raise GitHubError(f"unknown auth source: {source}")


class GitHub:
    def __init__(self, auth_source: str) -> None:
        self._auth_source = auth_source
        self._token: str | None = None
        self._session: aiohttp.ClientSession | None = None

    async def close(self) -> None:
        if self._session:
            await self._session.close()

    async def _request(self, method: str, url: str, *, retry_auth: bool = True, **kwargs) -> Any:
        if self._session is None:
            self._session = aiohttp.ClientSession()
        if self._token is None:
            self._token = await resolve_token(self._auth_source)
        headers = {
            "Authorization": f"Bearer {self._token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        }
        async with self._session.request(method, f"{API}{url}", headers=headers, **kwargs) as resp:
            if resp.status == 401 and retry_auth:
                # Token may have been rotated (e.g. `gh auth refresh`) — re-resolve once.
                self._token = None
                return await self._request(method, url, retry_auth=False, **kwargs)
            if resp.status == 204:
                return None
            body = await resp.json(content_type=None)
            if resp.status >= 400:
                raise GitHubError(f"{method} {url} -> {resp.status}: {body}")
            return body

    async def generate_jitconfig(
        self, repo: str, name: str, labels: list[str]
    ) -> tuple[int, str]:
        """Returns (runner_id, encoded_jit_config). The encoded config is
        credential material — pass it to the container env, never log it."""
        body = await self._request(
            "POST",
            f"/repos/{repo}/actions/runners/generate-jitconfig",
            json={
                "name": name,
                "runner_group_id": 1,  # personal repos only have the default group
                "labels": labels,
                "work_folder": "_work",
            },
        )
        return body["runner"]["id"], body["encoded_jit_config"]

    async def list_runners(self, repo: str) -> list[dict]:
        body = await self._request("GET", f"/repos/{repo}/actions/runners?per_page=100")
        return body.get("runners", [])

    async def delete_runner(self, repo: str, runner_id: int) -> None:
        await self._request("DELETE", f"/repos/{repo}/actions/runners/{runner_id}")

    async def find_job_by_runner(self, repo: str, runner_name: str) -> dict | None:
        """Dashboard enrichment after a runner turns busy: find the in-progress
        job assigned to it. One runs listing + its job listings; best-effort."""
        runs = await self._request(
            "GET", f"/repos/{repo}/actions/runs?status=in_progress&per_page=10"
        )
        for run in runs.get("workflow_runs", []):
            jobs = await self._request(
                "GET", f"/repos/{repo}/actions/runs/{run['id']}/jobs?per_page=100"
            )
            for job in jobs.get("jobs", []):
                if job.get("runner_name") == runner_name:
                    return {
                        "job_id": job["id"],
                        "run_id": run["id"],
                        "workflow": run.get("name", ""),
                        "job_name": job.get("name", ""),
                        "html_url": job.get("html_url", ""),
                    }
        return None

    async def get_job(self, repo: str, job_id: int) -> dict:
        return await self._request("GET", f"/repos/{repo}/actions/jobs/{job_id}")

    async def latest_runner_release(self) -> str:
        body = await self._request("GET", "/repos/actions/runner/releases/latest")
        return body["tag_name"].lstrip("v")

    async def rate_limit(self) -> dict:
        body = await self._request("GET", "/rate_limit")
        return body["resources"]["core"]
