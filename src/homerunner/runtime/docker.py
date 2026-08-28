"""Docker driver. Runners are --privileged so each can run its own inner
dockerd (services: blocks with hosted-runner localhost semantics)."""

from __future__ import annotations

import asyncio
import json
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


class DockerRuntime:
    name = "docker"

    async def available(self) -> str | None:
        try:
            await _run("docker", "info", "--format", "{{.ServerVersion}}")
            return None
        except (RuntimeUnavailable, FileNotFoundError) as exc:
            return f"docker daemon unreachable: {exc}"

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
            "docker", "run", "-d", "--privileged",
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
        out = await _run("docker", "wait", container_id)
        return int(out.strip())

    async def logs(self, container_id: str) -> AsyncIterator[str]:
        proc = await asyncio.create_subprocess_exec(
            "docker", "logs", "-f", container_id,
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

    async def list_managed(self) -> list[ManagedContainer]:
        out = await _run(
            "docker", "ps", "-a",
            "--filter", f"label={MANAGED_LABEL}=true",
            "--format", "{{json .}}",
        )
        containers: list[ManagedContainer] = []
        for line in out.splitlines():
            row = json.loads(line)
            inspect = json.loads(
                await _run("docker", "inspect", "--format", "{{json .}}", row["ID"])
            )
            labels = inspect["Config"]["Labels"] or {}
            state = inspect["State"]
            containers.append(
                ManagedContainer(
                    container_id=row["ID"],
                    runner_name=labels.get(RUNNER_LABEL, row.get("Names", "")),
                    repo=labels.get(REPO_LABEL, ""),
                    running=bool(state.get("Running")),
                    exit_code=None if state.get("Running") else int(state.get("ExitCode", -1)),
                )
            )
        return containers

    async def kill(self, container_id: str) -> None:
        await _run("docker", "kill", container_id, check=False)

    async def remove(self, container_id: str) -> None:
        # -v: drop the anonymous /var/lib/docker volume from the inner daemon.
        await _run("docker", "rm", "-f", "-v", container_id, check=False)
