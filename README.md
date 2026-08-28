# homerunner

Self-hosted GitHub Actions runners on a Mac. Keeps a small pool of ephemeral
runners per repo: each one takes a single job in its own container/VM, exits,
and gets replaced. Self-hosted minutes are free, and logs/checks show up in
GitHub like normal.

Three things make it more than a quota workaround:

- **CI state stays on your machine.** Step logs are captured from every job,
  and a failed job keeps its *entire workspace* — checkout, build state,
  tmpdirs — as a local image. `homerunner exec` opens a shell in the exact
  failed state, so a flaky test gets rerun in place instead of reasoned about
  from a log viewer. Hosted CI throws all of this away.
- **Built for agents.** `homerunner why` prints a failure digest (what ran,
  the log excerpt around the error, whether the workspace was kept), every
  query takes `--json`, and `homerunner mcp` serves the same queries as MCP
  tools — so a coding agent can debug CI without touching GitHub's API.
- **VM-per-job isolation** via [apple/container](https://github.com/apple/container)
  on Apple Silicon (not yet verified on hardware:
  [docs/arm64-verification.md](docs/arm64-verification.md)), with a
  `--privileged` Docker driver everywhere else.

Each runner gets a private dockerd, so `services:` blocks behave exactly like
GitHub-hosted runners.

> [!WARNING]
> Private repos only. On a public repo, fork PRs run arbitrary code on your
> machine.
>
> The `docker` runtime is **not a security boundary**: runners are
> `--privileged` (the inner dockerd requires it), and a privileged container
> can escape to the host by design. Treat every job as if it runs directly on
> your machine, and only point homerunner at code you'd run there yourself.
> The per-job VMs of `apple-container` are the stronger story, but shared
> caches and your local network are still exposed to whatever a job does.

## Setup

```sh
brew install gh && gh auth login      # token: repo scope
cargo install --path .
homerunner init --repo you/yourrepo   # writes config, builds the runner image, checks access
homerunner install                    # launchd agent (or `run` for foreground)
```

The Dockerfile is embedded in the binary, so `init` on a fresh machine needs
nothing but Docker (or apple/container) and `gh`. `init` picks defaults by
arch — `apple-container`/arm64 labels on Apple Silicon, `docker`/x64
otherwise — and leaves an existing config alone.

Then set `runs-on: [self-hosted, linux, x64]` in your workflows.

## Commands

| | |
|---|---|
| `run` | supervisor + dashboard (http://127.0.0.1:8123) in the foreground |
| `status` | pool summary |
| `jobs [--json]` | job history, with log/workspace availability |
| `why [job] [--json]` | failure digest: what ran, log excerpt around the error |
| `logs [job]` | full captured step logs (`latest`, `latest-failed`, or an id) |
| `exec [job]` | shell inside a kept failed-job workspace |
| `mcp` | MCP server over stdio: `claude mcp add homerunner -- homerunner mcp` |
| `doctor` | check token, runtime, image, repo access |
| `init` / `install` / `build-image` | one-time setup, launchd agent, image rebuild |

Failed jobs keep their workspace as a local image (`keep_failed_workspaces`,
default 2, oldest GC'd). The dashboard links each job's captured logs and
marks kept workspaces.

## Notes

- Per repo, `reserved` warm runners stay listening (0 = fully on-demand) and
  queued jobs burst the pool up to `max`. Warm pickups are push-latency; burst
  and on-demand pickups add up to `poll_interval_s` (default 30s). Idle burst
  runners decay after `idle_decay_min`. If `max == reserved` everywhere,
  nothing ever polls.
- Add a `concurrency` block with `cancel-in-progress` to your workflows, or
  superseded pushes hold runners.
- Rebuild the image when `actions/runner` releases move on — GitHub rejects
  runners that are a few versions stale (the supervisor logs the latest daily).
- Jobs queue while the Mac sleeps; `caffeinate = true` keeps it awake mid-job.
