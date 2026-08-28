# Verifying the apple/container runtime (Apple Silicon)

The `apple-container` driver has never executed — it was written on an Intel
Mac against apple/container 1.0 docs (the CLI is at 1.3.x now). This is the
checklist for its first run on arm64 hardware. Phases 1–2 need no Rust
toolchain; phase 3 exercises the driver itself.

## Phase 1 — CLI smoke test

```sh
brew install container
container system start          # first start downloads the default kernel
container system status
container run --rm alpine echo ok
```

While here, verify the CLI shapes the driver assumes (`src/runtime.rs`):

```sh
container run -d --name t1 --label homerunner.managed=true alpine sleep 60
container ls -a --format json   # does each row have id / status / labels{}?
container inspect t1            # top-level: status, exitCode? array or object?
container rm -f t1
```

If field names differ, fix `list_managed`/`wait` in `src/runtime.rs`
(the driver reads `id`, `status == "running"`, `exitCode`, `labels`).
Also check `container run` accepts: `-d`, `--cpus`, `--memory`, `--name`,
`--label` (repeated), `-e`, `-v name:/path` (named volumes), and that
`container logs -f` streams.

## Phase 2 — the experiment: dockerd inside the VM

Build the runner image with apple/container's own builder (the base image is
multi-arch, so arm64 resolves automatically):

```sh
container build -t homerunner-runner:local images/runner
```

Run one runner without a JIT config just to probe docker:

```sh
container run -d --name probe --cpus 4 --memory 6g homerunner-runner:local || true
# entrypoint will exit(64) fast on missing HOMERUNNER_JIT_CONFIG, so instead:
container run --rm --cpus 4 --memory 6g --entrypoint /bin/bash \
  homerunner-runner:local -c 'dockerd >/tmp/d.log 2>&1 & sleep 8; docker info && docker run --rm alpine echo DIND-OK; tail -5 /tmp/d.log'
```

- `DIND-OK` → the stock Apple kernel is enough. Done; record it here.
- dockerd dies → read the log tail. Expected suspects: overlayfs missing
  (storage driver errors), iptables/nftables missing (network controller
  errors). Next step is a custom guest kernel: the Containerization project
  documents building one; enable `OVERLAY_FS`, `BRIDGE`, `VETH`,
  `NETFILTER`/`NF_TABLES`/`IPTABLES`, cgroup bits, then point `container` at
  it (check current mechanism: per-run flag vs `container system` default).
- If that stalls, `runtime = "docker"` per repo is the working fallback.

Also measure: RAM an idle VM actually holds (sizes `pool_size`), and boot
latency vs Docker (`container run` to "Listening for Jobs").

## Phase 3 — driver end-to-end

```sh
cargo install --path .
homerunner init --repo you/scratch-repo   # on arm64 this defaults to
                                          # runtime = "apple-container" + arm64 labels
homerunner run
```

Point it at a scratch private repo with a trivial workflow using
`runs-on: [self-hosted, linux, arm64]`, push, and watch: spawn → listening →
busy → exit → respawn. Then a workflow with a postgres `services:` block for
the full DinD path. Note `wait()` polls `container inspect` every 2s (no
`container wait` existed at 1.0 — if one exists now, use it).

## Results

_(fill in after the first arm64 run)_
