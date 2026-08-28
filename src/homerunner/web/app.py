"""Localhost dashboard: state snapshot + SSE change feed + runner log tails.
Full step logs live in GitHub's Actions UI; this covers the infra side."""

from __future__ import annotations

import asyncio
import contextlib
import json
from pathlib import Path

from aiohttp import web

from ..github import GitHub, GitHubError
from ..scheduler import Scheduler
from ..store import Store

STATIC = Path(__file__).parent / "static"


class ChangeHub:
    """Fan-out for 'something changed' pokes; SSE clients refetch the snapshot."""

    def __init__(self) -> None:
        self._waiters: set[asyncio.Event] = set()

    def notify(self) -> None:
        for event in self._waiters:
            event.set()

    async def wait(self) -> None:
        event = asyncio.Event()
        self._waiters.add(event)
        try:
            await event.wait()
        finally:
            self._waiters.discard(event)


def build_app(scheduler: Scheduler, store: Store, github: GitHub, hub: ChangeHub) -> web.Application:
    app = web.Application()

    async def index(_request: web.Request) -> web.FileResponse:
        return web.FileResponse(STATIC / "index.html")

    async def state(_request: web.Request) -> web.Response:
        snapshot = scheduler.snapshot()
        snapshot["jobs"] = store.recent_jobs()
        snapshot["events"] = store.recent_events(50)
        return web.json_response(snapshot)

    async def rate(_request: web.Request) -> web.Response:
        try:
            return web.json_response(await github.rate_limit())
        except GitHubError as exc:
            return web.json_response({"error": str(exc)}, status=502)

    async def events(request: web.Request) -> web.StreamResponse:
        resp = web.StreamResponse(headers={"Content-Type": "text/event-stream", "Cache-Control": "no-cache"})
        await resp.prepare(request)
        with contextlib.suppress(ConnectionResetError, asyncio.CancelledError):
            while True:
                await hub.wait()
                await resp.write(b"data: changed\n\n")
        return resp

    async def runner_logs(request: web.Request) -> web.StreamResponse:
        name = request.match_info["name"]
        runner = scheduler.runners.get(name)
        if runner is None:
            raise web.HTTPNotFound(text=f"no live runner {name}")
        runtime = scheduler._runtimes[runner.repo_cfg.runtime]
        resp = web.StreamResponse(headers={"Content-Type": "text/event-stream", "Cache-Control": "no-cache"})
        await resp.prepare(request)
        with contextlib.suppress(ConnectionResetError, asyncio.CancelledError):
            async for line in runtime.logs(runner.container_id):
                await resp.write(f"data: {json.dumps(line)}\n\n".encode())
        return resp

    app.router.add_get("/", index)
    app.router.add_get("/api/state", state)
    app.router.add_get("/api/rate", rate)
    app.router.add_get("/events", events)
    app.router.add_get("/api/runners/{name}/logs", runner_logs)
    return app


async def serve(app: web.Application, port: int) -> web.AppRunner:
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", port)
    await site.start()
    return runner
