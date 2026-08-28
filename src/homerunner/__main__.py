from __future__ import annotations

import argparse
import asyncio
import json
import plistlib
import subprocess
import sys
import urllib.request
from pathlib import Path

from .config import Config, ConfigError, load
from .github import GitHub, GitHubError
from .runtime import get_runtime
from .scheduler import Scheduler
from .store import Store
from .web.app import ChangeHub, build_app, serve

DEFAULT_CONFIG = Path("~/.config/homerunner/config.toml").expanduser()
PLIST_LABEL = "dev.highstorm.homerunner"


async def cmd_run(config: Config) -> None:
    github = GitHub(config.auth_source)
    store = Store(config.db_path)
    hub = ChangeHub()
    scheduler = Scheduler(config, github, store, on_change=hub.notify)
    await scheduler.start()
    app = build_app(scheduler, store, github, hub)
    runner = await serve(app, config.dashboard_port)
    print(f"[web] dashboard on http://127.0.0.1:{config.dashboard_port}", flush=True)
    try:
        await asyncio.Event().wait()
    finally:
        await scheduler.stop()
        await runner.cleanup()
        await github.close()
        store.close()


def cmd_status(config: Config) -> int:
    try:
        with urllib.request.urlopen(
            f"http://127.0.0.1:{config.dashboard_port}/api/state", timeout=3
        ) as resp:
            data = json.load(resp)
    except OSError:
        print("supervisor not running (dashboard unreachable)")
        return 1
    for repo in data["repos"]:
        print(f"{repo['repo']}: {repo['live']}/{repo['pool_size']} live ({repo['runtime']})")
    for r in data["runners"]:
        job = r["job"].get("job_name", "")
        print(f"  {r['name']}  {r['state']}" + (f"  {job}" if job else ""))
    for name, reason in (data.get("degraded") or {}).items():
        print(f"DEGRADED {name}: {reason}")
    return 0


async def cmd_doctor(config: Config) -> int:
    ok = True
    for runtime_name in {rc.runtime for rc in config.repos}:
        reason = await get_runtime(runtime_name).available()
        print(f"runtime {runtime_name}: {'ok' if reason is None else reason}")
        ok = ok and reason is None
    github = GitHub(config.auth_source)
    try:
        core = await github.rate_limit()
        print(f"github token: ok ({core['remaining']}/{core['limit']} requests remaining)")
        for rc in config.repos:
            try:
                runners = await github.list_runners(rc.repo)
                print(f"repo {rc.repo}: ok ({len(runners)} registered runner(s))")
            except GitHubError as exc:
                print(f"repo {rc.repo}: FAIL {exc}")
                ok = False
    except GitHubError as exc:
        print(f"github token: FAIL {exc}")
        ok = False
    finally:
        await github.close()
    images = {rc.image for rc in config.repos if rc.runtime == "docker"}
    for image in images:
        found = (
            subprocess.run(
                ["docker", "image", "inspect", image], capture_output=True
            ).returncode
            == 0
        )
        print(f"image {image}: {'ok' if found else 'MISSING (scripts/build-image.sh)'}")
        ok = ok and found
    return 0 if ok else 1


def cmd_install(config_path: Path) -> None:
    plist_path = Path(f"~/Library/LaunchAgents/{PLIST_LABEL}.plist").expanduser()
    log_dir = Path("~/Library/Logs/homerunner").expanduser()
    log_dir.mkdir(parents=True, exist_ok=True)
    plist = {
        "Label": PLIST_LABEL,
        "ProgramArguments": [
            sys.executable, "-m", "homerunner", "run", "--config", str(config_path),
        ],
        "RunAtLoad": True,
        "KeepAlive": {"SuccessfulExit": False},
        "StandardOutPath": str(log_dir / "homerunner.log"),
        "StandardErrorPath": str(log_dir / "homerunner.log"),
        # launchd's PATH is bare; docker + gh live here.
        "EnvironmentVariables": {
            "PATH": "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        },
    }
    plist_path.write_bytes(plistlib.dumps(plist))
    uid = subprocess.run(["id", "-u"], capture_output=True, text=True).stdout.strip()
    subprocess.run(["launchctl", "bootout", f"gui/{uid}/{PLIST_LABEL}"], capture_output=True)
    subprocess.run(["launchctl", "bootstrap", f"gui/{uid}", str(plist_path)], check=True)
    print(f"installed + started {PLIST_LABEL}")
    print(f"logs: {log_dir}/homerunner.log")


def main() -> None:
    parser = argparse.ArgumentParser(prog="homerunner")
    parser.add_argument("command", choices=["run", "status", "doctor", "install"])
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    args = parser.parse_args()

    if args.command == "install":
        cmd_install(args.config)
        return

    try:
        config = load(args.config)
    except ConfigError as exc:
        print(f"config error: {exc}", file=sys.stderr)
        sys.exit(2)

    if args.command == "run":
        try:
            asyncio.run(cmd_run(config))
        except KeyboardInterrupt:
            pass  # runners stay up; next start re-adopts them
    elif args.command == "status":
        sys.exit(cmd_status(config))
    elif args.command == "doctor":
        sys.exit(asyncio.run(cmd_doctor(config)))


if __name__ == "__main__":
    main()
