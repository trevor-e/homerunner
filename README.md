# homerunner

Runs GitHub Actions jobs on your own Mac, using Apple's
[container](https://github.com/apple/container) stack (or Docker) for
isolation. GitHub doesn't meter self-hosted runner minutes and handles
dispatch, live logs, and status checks the same as for hosted runners, so CI
keeps working when you're out of quota. The only workflow change is the
`runs-on` labels.

The interesting part is the runtime. With apple/container, every job gets its
own lightweight VM with its own kernel via the Virtualization framework —
stronger isolation than sharing one Docker daemon, and a clean machine per
job. A Docker driver provides the same behavior with `--privileged`
containers; it's the fallback while the apple/container driver matures, and
the only option on Intel Macs.

Honest status: the Docker driver is what's battle-tested (it has run
thousands of job-minutes of real CI, including Postgres `services:` blocks
and Playwright browser jobs). The apple/container driver is written to the
same interface but hasn't run yet — it needs an Apple Silicon machine, and
running dockerd inside the VM may need Apple's minimal guest kernel rebuilt
with overlayfs/netfilter enabled (`container` accepts a custom kernel).

Some mechanics worth knowing:

- Runners are single-use. Each one is registered just-in-time, takes one job,
  exits, and is replaced by a fresh registration. The supervisor keeps
  `pool_size` of them alive per repo; that number is also your concurrency
  cap, and jobs beyond it queue on GitHub's side. GitHub delivers jobs over a
  connection the runner itself holds open, so the supervisor never has to
  poll the API for work — it just reacts to containers exiting.
- Each runner starts a private dockerd inside its own container/VM. That's
  what makes `services:` blocks behave like GitHub-hosted runners:
  `localhost:<port>` resolves inside the job, and two concurrent jobs
  publishing the same port don't collide. Don't mount the host Docker socket
  instead; both properties break.
- Named volumes keep the uv cache, pnpm store, and Playwright browsers warm
  across jobs, which is most of why local runs end up faster than hosted.

**Only use this with private repos.** A public repo would let fork PRs run
arbitrary code on your machine.

## Setup

```sh
brew install gh && gh auth login   # token needs repo scope, or fine-grained
                                   # Actions:read + Administration:read/write
scripts/build-image.sh             # builds homerunner-runner:local
cp config.example.toml ~/.config/homerunner/config.toml   # edit repos
cargo build --release
target/release/homerunner doctor   # token, runtime, image, repo access
target/release/homerunner run      # foreground; dashboard on 127.0.0.1:8123
target/release/homerunner install  # launchd agent, starts at login
```

Then change workflows to `runs-on: [self-hosted, linux, x64]` (self-hosted
runners can't claim `ubuntu-latest`). Jobs queue while the Mac sleeps and run
on wake; `caffeinate = true` keeps it awake while a job is mid-flight. A
`concurrency` block with `cancel-in-progress` in your workflows is worth
adding — with a small pool, a stale run from a superseded push holds a
runner you'd rather have back.

## Commands

- `run` — supervisor plus dashboard in the foreground. Ctrl-C leaves runner
  containers alive; the next start re-adopts them (even mid-job), reaps
  finished ones, and deletes orphaned registrations.
- `status` — one-shot summary from the dashboard API.
- `doctor` — token, runtime, image, and per-repo access checks.
- `install` — writes and bootstraps
  `~/Library/LaunchAgents/dev.highstorm.homerunner.plist`; logs to
  `~/Library/Logs/homerunner/`.

## Dashboard

`http://127.0.0.1:8123`: pool health per repo, live runners with their
current job linked to the GitHub run, recent history, and a rolling tail of
runner diagnostics. Step logs live in GitHub's Actions UI like always.

## Gotchas learned the hard way

- The runner base image is pinned in `images/runner/Dockerfile`. GitHub
  refuses runners more than a few versions stale; the supervisor logs the
  latest release daily, and the fix is rebuilding the image.
- Docker creates missing volume mountpoints as root, which silently breaks
  uv and pnpm writing to `~/.local` and `~/.cache`. The image pre-creates
  them owned by `runner`.
- Don't share a `RUNNER_TOOL_CACHE` volume between runners. Concurrent
  setup-* actions race populating it (one runner sees another's
  half-extracted node and skips the download). Per-job `_work/_tool` is what
  hosted runners do, for good reason.
- pip-audit and friends need `python3-venv` from the image; Ubuntu's bare
  python3 can't build venvs.
