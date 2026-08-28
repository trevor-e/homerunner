FROM ghcr.io/actions/actions-runner:2.337.0

USER root
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
      git curl jq zstd unzip xz-utils ca-certificates gnupg \
      build-essential sudo iptables util-linux \
      python3 python3-venv python3-pip \
    && echo "runner ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/runner \
    && chmod 0440 /etc/sudoers.d/runner \
    && rm -rf /var/lib/apt/lists/*

# Docker engine: each runner container runs a private dockerd (started by the
# entrypoint) so `services:` blocks get GitHub-hosted localhost semantics and
# concurrent jobs can't collide on published ports.
RUN install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu noble stable" \
       > /etc/apt/sources.list.d/docker.list \
    && apt-get update && apt-get install -y --no-install-recommends \
      docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin \
    && usermod -aG docker runner \
    && rm -rf /var/lib/apt/lists/*

# Playwright chromium system deps baked in so `playwright install --with-deps`
# has nothing left to apt-install (sudo covers stragglers).
RUN apt-get update && apt-get install -y --no-install-recommends \
      libasound2t64 libatk-bridge2.0-0 libatk1.0-0 libatspi2.0-0 libcairo2 \
      libcups2 libdbus-1-3 libdrm2 libgbm1 libnspr4 libnss3 libpango-1.0-0 \
      libwayland-client0 libx11-6 libxcb1 libxcomposite1 libxdamage1 libxext6 \
      libxfixes3 libxkbcommon0 libxrandr2 fonts-liberation fonts-unifont \
    && rm -rf /var/lib/apt/lists/*

# Cache-volume mountpoints must pre-exist owned by runner: Docker creates
# missing mountpoint parents as root, which breaks uv/pnpm writing to
# ~/.local and ~/.cache. (No shared toolcache volume: setup-* actions race
# when concurrent runners populate one toolcache; the per-job _work/_tool
# default is what hosted runners do.)
RUN mkdir -p /home/runner/.cache /home/runner/.local/share/pnpm/store \
    && chown -R runner:runner /home/runner/.cache /home/runner/.local

# The inner dockerd cannot nest overlayfs on the container's own overlay FS.
VOLUME /var/lib/docker

COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
