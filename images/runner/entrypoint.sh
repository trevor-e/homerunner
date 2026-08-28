#!/usr/bin/env bash
set -uo pipefail

# cgroup v2 nesting setup, same dance as the official docker:dind entrypoint:
# move root-cgroup processes into an init group and delegate controllers.
if [ -f /sys/fs/cgroup/cgroup.controllers ]; then
  mkdir -p /sys/fs/cgroup/init
  xargs -rn1 < /sys/fs/cgroup/cgroup.procs > /sys/fs/cgroup/init/cgroup.procs 2>/dev/null || true
  sed -e 's/ / +/g' -e 's/^/+/' < /sys/fs/cgroup/cgroup.controllers \
    > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || true
fi

dockerd_args=(--host=unix:///var/run/docker.sock)
if [ -n "${REGISTRY_MIRROR:-}" ]; then
  dockerd_args+=(--registry-mirror="$REGISTRY_MIRROR")
fi
dockerd "${dockerd_args[@]}" >/var/log/dockerd.log 2>&1 &

for _ in $(seq 1 30); do
  docker info >/dev/null 2>&1 && break
  sleep 1
done
if ! docker info >/dev/null 2>&1; then
  # Degrade rather than die: jobs without services:/container: still run fine.
  echo "WARNING: inner dockerd failed to start; services: blocks will fail" >&2
  tail -n 20 /var/log/dockerd.log >&2 || true
fi

# Volumes created by an older image may be root-owned; top-level chown is
# enough (contents written by runner stay runner-owned).
chown runner:runner /home/runner/.cache \
  /home/runner/.local /home/runner/.local/share /home/runner/.local/share/pnpm \
  /home/runner/.local/share/pnpm/store 2>/dev/null || true

if [ -z "${HOMERUNNER_JIT_CONFIG:-}" ]; then
  echo "HOMERUNNER_JIT_CONFIG not set" >&2
  exit 64
fi

exec runuser -u runner -- /home/runner/run.sh --jitconfig "$HOMERUNNER_JIT_CONFIG"
