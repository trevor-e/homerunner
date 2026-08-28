"""Runtime protocol: the container engine a runner pool spawns into.

Drivers shell out to their CLI via asyncio subprocesses; no daemon SDKs.
Containers are tagged with labels so any homerunner process can re-discover
its runners after a crash:
  homerunner.managed=true
  homerunner.repo=<owner/name>
  homerunner.runner=<runner name>
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import AsyncIterator, Protocol

MANAGED_LABEL = "homerunner.managed"
REPO_LABEL = "homerunner.repo"
RUNNER_LABEL = "homerunner.runner"

# Named volumes shared across runners: warm tool/dependency caches are where
# self-hosted beats GitHub-hosted on wall time. (uv + pnpm stores are
# concurrency-safe.)
CACHE_VOLUMES = {
    "homerunner-toolcache": "/opt/hostedtoolcache",
    "homerunner-home-cache": "/home/runner/.cache",
    "homerunner-pnpm-store": "/home/runner/.local/share/pnpm/store",
}


@dataclass(frozen=True)
class ManagedContainer:
    container_id: str
    runner_name: str
    repo: str
    running: bool
    exit_code: int | None


class RuntimeUnavailable(Exception):
    pass


class Runtime(Protocol):
    name: str

    async def available(self) -> str | None:
        """None if usable, else a human-readable reason it isn't."""
        ...

    async def spawn(
        self,
        *,
        runner_name: str,
        repo: str,
        image: str,
        jit_config: str,
        registry_mirror: str | None,
    ) -> str:
        """Start a runner container; returns container id."""
        ...

    async def wait(self, container_id: str) -> int:
        """Block until the container exits; returns exit code."""
        ...

    async def logs(self, container_id: str) -> AsyncIterator[str]:
        """Follow the container's stdout/stderr line stream."""
        ...

    async def list_managed(self) -> list[ManagedContainer]: ...

    async def kill(self, container_id: str) -> None: ...

    async def remove(self, container_id: str) -> None: ...
