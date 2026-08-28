# homerunner

A warm pool of ephemeral, self-hosted GitHub Actions runners on your own Mac —
because self-hosted runner minutes are free and unlimited, and GitHub already
handles dispatch, live logs, and status sync for you.

**No polling anywhere.** Registered runners hold their own outbound long-poll
to GitHub's broker and get jobs *pushed* to them. homerunner just keeps
`pool_size` single-job (JIT/ephemeral) runners listening per repo, and reacts
to *local* container-exit events: a runner takes exactly one job, exits, and a
fresh one with a new just-in-time registration replaces it. Clean environment
per job, zero REST polling, push-latency pickups.

Each runner container runs a **private inner dockerd**, so workflow
`services:` blocks (e.g. Postgres with published ports) behave exactly like
GitHub-hosted runners — `localhost:<port>` works, and concurrent jobs can't
collide on ports. Shared named volumes keep uv/pnpm/toolcache/Playwright
caches warm across jobs, which is where a local runner beats hosted speed.

> **Private repos only.** Never point this at a public repo: fork PRs would
> run arbitrary code on your machine.

## Setup

```sh
brew install gh && gh auth login       # token needs repo scope (or fine-grained: Actions read + Administration read/write)
scripts/build-image.sh                 # build homerunner-runner:local (Docker Desktop must be running)
cp config.example.toml ~/.config/homerunner/config.toml   # then edit repos
uv run homerunner doctor               # verify token, runtime, image, repo access
uv run homerunner run                  # foreground; dashboard at http://127.0.0.1:8123
uv run homerunner install             # or: launchd agent, starts at login
```

Then flip workflows to `runs-on: [self-hosted, linux, x64]` (self-hosted
runners can't claim `ubuntu-latest`). Jobs queue while the Mac is asleep and
run on wake; `caffeinate = true` keeps it awake while a job is mid-flight.

## Commands

- `homerunner run` — supervisor + dashboard in the foreground. Ctrl-C leaves
  runner containers alive; the next start re-adopts them, reaps finished ones,
  and deletes orphaned GitHub registrations.
- `homerunner status` — one-shot pool/runner summary from the dashboard API.
- `homerunner doctor` — token, runtime, image, and per-repo access checks.
- `homerunner install` — writes and bootstraps the launchd agent
  (`~/Library/LaunchAgents/dev.highstorm.homerunner.plist`, logs in
  `~/Library/Logs/homerunner/`).

## Dashboard

`http://127.0.0.1:8123` — pool health per repo, live runners with their
current job (linked to the GitHub run), recent job history, and a rolling
runner-log tail. SSE-driven; full step logs stay in GitHub's Actions UI.

## Runtimes

`runtime = "docker"` (default) runs runners as `--privileged` Docker
containers with dockerd-in-Docker. `runtime = "apple-container"` is the
intended future default on Apple Silicon (one lightweight VM per runner via
apple/container; requires arm64 — the driver reports why when it can't run).

## Notes

- The runner base image is pinned in `images/runner/Dockerfile`; GitHub
  refuses runners more than a few versions stale. The supervisor logs the
  latest release daily — rebuild the image when it drifts.
- Cache-volume mountpoints are pre-created in the image owned by `runner`
  (Docker creates missing mountpoints as root, which breaks uv/pnpm), and
  `RUNNER_TOOL_CACHE=/opt/hostedtoolcache` points setup-* actions at the
  shared toolcache volume.
