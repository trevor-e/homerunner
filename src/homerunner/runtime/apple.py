"""apple/container driver — requires an Apple Silicon Mac.

Kept as the intended primary runtime for when homerunner moves to (or gains)
arm64 hardware; on Intel `available()` reports why it can't run. The CLI
mapping mirrors DockerRuntime; flag syntax should be re-verified against the
installed `container` release before first use (labels/format/json output).
Remember: apple/container defaults each VM to 1GB RAM — always pass --memory.
"""

from __future__ import annotations

import asyncio
import json
import platform
import shutil
from typing import AsyncIterator

from .base import (
    CACHE_VOLUMES,
    MANAGED_LABEL,
    REPO_LABEL,
    RUNNER_LABEL,
    ManagedContainer,
    RuntimeUnavailable,
)


async def _run(*argv: str, check: bool = True) -> str:
    proc = await asyncio.create_subprocess_exec(
        *argv, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
    )
    out, err = await proc.communicate()
    if check and proc.returncode != 0:
        raise RuntimeUnavailable(f"{' '.join(argv[:3])}... failed: {err.decode().strip()}")
    return out.decode()


class AppleContainerRuntime:
    name = "apple-container"

    def __init__(self, cpus: int = 4, memory: str = "6g") -> None:
        self.cpus = cpus
        self.memory = memory

    async def available(self) -> str | None:
        if platform.machine() != "arm64":
            return "apple/container requires an Apple Silicon Mac (this machine is Intel)"
        if shutil.which("container") is None:
            return "`container` CLI not installed (brew install container)"
        try:
            await _run("container", "system", "status")
            return None
        except RuntimeUnavailable as exc:
            return f"container system not running (`container system start`): {exc}"

    async def spawn(
        self,
        *,
        runner_name: str,
        repo: str,
        image: str,
        jit_config: str,
        registry_mirror: str | None,
    ) -> str:
        argv = [
            "container", "run", "-d",
            "--cpus", str(self.cpus),
            "--memory", self.memory,
            "--name", runner_name,
            "--label", f"{MANAGED_LABEL}=true",
            "--label", f"{REPO_LABEL}={repo}",
            "--label", f"{RUNNER_LABEL}={runner_name}",
            "-e", f"HOMERUNNER_JIT_CONFIG={jit_config}",
        ]
        if registry_mirror:
            argv += ["-e", f"REGISTRY_MIRROR={registry_mirror}"]
        for volume, mount in CACHE_VOLUMES.items():
            argv += ["-v", f"{volume}:{mount}"]
        argv.append(image)
        out = await _run(*argv)
        return out.strip()

    async def wait(self, container_id: str) -> int:
        # `container` has no `wait`; poll inspect until the VM stops.
        while True:
            info = await self._inspect(container_id)
            if info is not None and info.get("status") != "running":
                return int(info.get("exitCode", -1))
            await asyncio.sleep(2)

    async def logs(self, container_id: str) -> AsyncIterator[str]:
        proc = await asyncio.create_subprocess_exec(
            "container", "logs", "-f", container_id,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
        )
        assert proc.stdout is not None
        try:
            async for raw in proc.stdout:
                yield raw.decode(errors="replace").rstrip("\n")
        finally:
            if proc.returncode is None:
                proc.kill()
                await proc.wait()

    async def _inspect(self, container_id: str) -> dict | None:
        out = await _run("container", "inspect", container_id, check=False)
        try:
            data = json.loads(out)
        except json.JSONDecodeError:
            return None
        return data[0] if isinstance(data, list) and data else None

    async def list_managed(self) -> list[ManagedContainer]:
        out = await _run("container", "ls", "-a", "--format", "json", check=False)
        containers: list[ManagedContainer] = []
        try:
            rows = json.loads(out)
        except json.JSONDecodeError:
            return containers
        for row in rows:
            labels = row.get("labels", {}) or {}
            if labels.get(MANAGED_LABEL) != "true":
                continue
            running = row.get("status") == "running"
            containers.append(
                ManagedContainer(
                    container_id=row.get("id", ""),
                    runner_name=labels.get(RUNNER_LABEL, ""),
                    repo=labels.get(REPO_LABEL, ""),
                    running=running,
                    exit_code=None if running else int(row.get("exitCode", -1)),
                )
            )
        return containers

    async def kill(self, container_id: str) -> None:
        await _run("container", "kill", container_id, check=False)

    async def remove(self, container_id: str) -> None:
        await _run("container", "rm", "-f", container_id, check=False)
