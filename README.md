# homerunner

**Use your beefy dev computers to run fast, agent-friendly GitHub Actions
without burning hosted-runner minutes.**

Homerunner keeps a warm pool of ephemeral self-hosted runners for your private
repos. Jobs and checks still behave like normal GitHub Actions, but they run on
hardware you already own, with local caches, searchable logs, and debuggable
workspaces. Your personal projects keep running even after your GitHub-hosted
runner quota is gone.

Avoiding quota is the immediate win. The second is a much tighter optimization
loop: see which jobs are slow or flaky, inspect them locally, fix the bottleneck,
and measure the next run. We used that loop to get a real CI suite down to about
a minute.

## Why homerunner

- **Keep CI running.** Jobs use your machines instead of consuming
  GitHub-hosted runner minutes—especially useful for personal projects.
- **Fast feedback.** Warm runners avoid cold starts, queued jobs burst across
  available capacity, and isolated Docker layer caches speed up repeat builds.
- **Optimization you can see.** The dashboard tracks duration, pass rate,
  memory, OOMs, and regressions across runs. Global search makes retained logs
  easy to explore.
- **Debug the real failure.** Every job keeps its step logs. Failed jobs can
  keep their entire workspace, so `homerunner exec` reopens the exact checkout,
  build state, and temp files that failed.
- **Built for agents.** CLI commands support JSON, `homerunner mcp` exposes the
  same data as tools, and [`skills/homerunner`](skills/homerunner/SKILL.md)
  packages the debugging workflow for coding agents.
- **Fresh job isolation.** Each runner handles one job in its own container or
  VM, gets replaced afterward, and has a private dockerd so `services:` works
  like it does on GitHub-hosted runners.

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

Download the archive for your platform from the
[latest GitHub release](https://github.com/trevor-e/homerunner/releases/latest),
extract it, and put `homerunner` (or `homerunner.exe`) somewhere on your
`PATH`. Release assets are available for Apple Silicon Macs, Intel Macs, and
64-bit Windows, with a SHA-256 checksum beside each archive.

On macOS, `/usr/local/bin` gives the launchd agent a stable executable path:

```sh
sudo install homerunner /usr/local/bin/homerunner
```

Then configure the runtime:

```sh
brew install gh && gh auth login      # token: repo scope
homerunner init --repo you/yourrepo   # writes config, builds the runner image, checks access
homerunner install                    # launchd agent (or `run` for foreground)
```

Windows support is currently experimental and requires Docker Desktop using
Linux containers. Run `homerunner run` in the foreground or arrange it with
your preferred service manager; `homerunner install` is launchd-only. To build
from source instead, install Rust and run `cargo install --path .`.

The Dockerfile is embedded in the binary, so `init` on a fresh machine needs
nothing but Docker (or apple/container) and `gh`. `init` picks defaults by
arch — `apple-container`/arm64 labels on Apple Silicon, `docker`/x64
otherwise — and leaves an existing config alone.

The runner image includes configurable Python and Node versions in the Actions
tool cache, so `actions/setup-python` and `actions/setup-node` can select them
without downloading a toolchain for every ephemeral job. Other versions remain
installable in the job's private container layer. The defaults are Python
3.13.1 and Node 24.14.0; change them in the config and run `homerunner
build-image` to rebuild:

```toml
[toolchains]
python = "3.13.1"
node = "24.14.0"
```

The image also includes common `ubuntu-latest` utilities and native build
prerequisites such as GitHub CLI, Git LFS, SSH, rsync, CMake, Ninja,
`pkg-config`, OpenSSL headers, and compression tools.

Then set `runs-on: [self-hosted, linux, x64]` in your workflows.

## Commands

| | |
|---|---|
| `run` | supervisor + dashboard (http://127.0.0.1:4123) in the foreground |
| `status` | pool summary |
| `jobs [--json]` | job history, with log/workspace availability |
| `analytics [--repo …] [--workflow …] [--job-name …] [--since 30d] [--json]` | cross-run pass-rate, duration, CPU/memory, OOM, and regression analysis |
| `why [job] [--json]` | failure digest: what ran, log excerpt around the error |
| `logs [job]` | full captured step logs (`latest`, `latest-failed`, or an id) |
| `search <query> [--since 30d] [--json]` | search retained logs across jobs with repo/workflow/branch/step/severity filters |
| `steps [job] [--json]` | detected step line ranges and error/warning counts |
| `exec [job] [-- cmd]` | shell inside a kept failed-job workspace; `-- cmd` runs one command, no TTY (for agents/scripts) |
| `live [runner-or-job] [-- cmd]` | attach to a live runner (default: latest busy runner), or run one command without a TTY |
| `gc [--dry-run] [--json]` | reconcile retained artifacts and enforce cleanup policy |
| `events [--json]` | follow the live event stream (`job_started`, `job_result`, `burst`, …; NDJSON with `--json`) |
| `mcp` | MCP server over stdio: `claude mcp add homerunner -- homerunner mcp` |
| `doctor` | check token, runtime, image, repo access |
| `init` / `install` / `build-image` | one-time setup, launchd agent, image rebuild |

## Operations

- The dashboard at [http://127.0.0.1:4123](http://127.0.0.1:4123) shows live
  capacity, runners, job history, analytics, retained logs, and global search.
- Failed Docker jobs can keep their workspace as a local image
  (`keep_failed_workspaces`, default 2). `homerunner exec` opens it and
  `homerunner gc --dry-run` previews cleanup.
- Optional `[[monitors]]` rules flag slow, repeatedly failing, OOM-killed, or
  regex-matching jobs. Rules can retain the matched workspace; see
  `config.example.toml`.
- `docker_layer_cache = true` keeps build layers in an isolated volume per repo
  concurrency slot. Containers and networks are still cleared between jobs.
  Only enable it where jobs are allowed to reuse one another's build layers.

Cleanup runs at startup, after completed jobs, and daily. Log, workspace,
metadata, event, cache, and service-log retention are all bounded in config.

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

## Development

Run the automated suite and strict lint checks with:

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

## Related: local-ci

[local-ci](https://github.com/redwoodjs/local-ci) runs workflows against your
dirty working tree before you push. Homerunner runs the real post-push GitHub
checks on your hardware. They compose well: local-ci is the preflight;
homerunner is the fast gatekeeper.
