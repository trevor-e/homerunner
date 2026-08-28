# homerunner

Self-hosted GitHub Actions runners on a Mac. Keeps a small pool of ephemeral
runners per repo: each one takes a single job in its own container/VM, exits,
and gets replaced. Self-hosted minutes are free, and logs/checks show up in
GitHub like normal.

Two runtimes:

- `apple-container` — one lightweight VM per job (Apple Silicon). Not yet
  verified on real hardware: [docs/arm64-verification.md](docs/arm64-verification.md)
- `docker` — privileged containers, works everywhere Docker does

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
brew install gh && gh auth login    # token: repo scope
scripts/build-image.sh
cp config.example.toml ~/.config/homerunner/config.toml
cargo build --release
target/release/homerunner doctor
target/release/homerunner install   # launchd agent (or `run` for foreground)
```

Then set `runs-on: [self-hosted, linux, x64]` in your workflows.

## Commands

| | |
|---|---|
| `run` | supervisor + dashboard (http://127.0.0.1:8123) in the foreground |
| `status` | pool summary |
| `doctor` | check token, runtime, image, repo access |
| `install` | launchd agent, starts at login |

## Notes

- `pool_size` is the per-repo concurrency cap; extra jobs queue on GitHub.
- Add a `concurrency` block with `cancel-in-progress` to your workflows, or
  superseded pushes hold runners.
- Rebuild the image when `actions/runner` releases move on — GitHub rejects
  runners that are a few versions stale (the supervisor logs the latest daily).
- Jobs queue while the Mac sleeps; `caffeinate = true` keeps it awake mid-job.
