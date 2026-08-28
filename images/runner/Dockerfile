FROM ghcr.io/actions/actions-runner:2.337.0

USER root
ENV DEBIAN_FRONTEND=noninteractive
ENV AGENT_TOOLSDIRECTORY=/opt/hostedtoolcache

RUN apt-get update && apt-get install -y --no-install-recommends \
      git git-lfs gh openssh-client curl wget jq rsync file patch \
      tar gzip bzip2 zstd zip unzip xz-utils ca-certificates gnupg locales \
      build-essential cmake ninja-build pkg-config libssl-dev libffi-dev \
      sudo iptables util-linux python3 python3-venv python3-pip \
    && echo "runner ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/runner \
    && chmod 0440 /etc/sudoers.d/runner \
    && rm -rf /var/lib/apt/lists/*

# Keep setup-python fast without a shared writable tool cache. Resolve the
# publisher's current artifact for the configured version and verify it using
# the checksums from the same release.
ARG PYTHON_VERSION=3.13.1
RUN set -eux; \
    case "$(dpkg --print-architecture)" in \
      arm64) python_arch=arm64 ;; \
      amd64) python_arch=x64 ;; \
      *) \
        echo "unsupported Python tool-cache architecture: $(dpkg --print-architecture)" >&2; \
        exit 1 \
        ;; \
    esac; \
    curl -fsSL --retry 3 \
      https://raw.githubusercontent.com/actions/python-versions/main/versions-manifest.json \
      -o /tmp/python-versions.json; \
    download_url="$(jq -r \
      --arg version "${PYTHON_VERSION}" \
      --arg arch "${python_arch}" \
      '.[] | select(.version == $version) | .files[] | select(.platform == "linux" and .platform_version == "24.04" and .arch == $arch) | .download_url' \
      /tmp/python-versions.json)"; \
    test -n "${download_url}"; \
    test "${download_url}" != null; \
    archive="${download_url##*/}"; \
    checksum_url="${download_url%/*}/hashes.sha256"; \
    curl -fL --retry 3 "${download_url}" -o "/tmp/${archive}"; \
    curl -fsSL --retry 3 "${checksum_url}" -o /tmp/python-hashes.sha256; \
    python_sha256="$(awk -v archive="${archive}" '$2 == archive { print $1 }' /tmp/python-hashes.sha256)"; \
    test -n "${python_sha256}"; \
    echo "${python_sha256}  /tmp/${archive}" | sha256sum --check; \
    mkdir /tmp/python-toolcache; \
    tar -xzf "/tmp/${archive}" -C /tmp/python-toolcache; \
    cd /tmp/python-toolcache; \
    bash ./setup.sh; \
    test -f "${AGENT_TOOLSDIRECTORY}/Python/${PYTHON_VERSION}/${python_arch}.complete"; \
    "${AGENT_TOOLSDIRECTORY}/Python/${PYTHON_VERSION}/${python_arch}/bin/python" \
      --version; \
    chown -R runner:runner "${AGENT_TOOLSDIRECTORY}"; \
    rm -rf "/tmp/${archive}" /tmp/python-toolcache \
      /tmp/python-versions.json /tmp/python-hashes.sha256

# setup-node uses the same tool-cache convention. Node publishes architecture-
# specific archives and release checksum lists at stable URLs.
ARG NODE_VERSION=24.14.0
RUN set -eux; \
    case "$(dpkg --print-architecture)" in \
      arm64) node_arch=arm64 ;; \
      amd64) node_arch=x64 ;; \
      *) \
        echo "unsupported Node tool-cache architecture: $(dpkg --print-architecture)" >&2; \
        exit 1 \
        ;; \
    esac; \
    archive="node-v${NODE_VERSION}-linux-${node_arch}.tar.xz"; \
    release_url="https://nodejs.org/dist/v${NODE_VERSION}"; \
    curl -fL --retry 3 "${release_url}/${archive}" -o "/tmp/${archive}"; \
    curl -fsSL --retry 3 "${release_url}/SHASUMS256.txt" -o /tmp/node-hashes.sha256; \
    node_sha256="$(awk -v archive="${archive}" '$2 == archive { print $1 }' /tmp/node-hashes.sha256)"; \
    test -n "${node_sha256}"; \
    echo "${node_sha256}  /tmp/${archive}" | sha256sum --check; \
    node_toolcache="${AGENT_TOOLSDIRECTORY}/node/${NODE_VERSION}/${node_arch}"; \
    mkdir -p "${node_toolcache}"; \
    tar -xJf "/tmp/${archive}" --strip-components=1 -C "${node_toolcache}"; \
    touch "${AGENT_TOOLSDIRECTORY}/node/${NODE_VERSION}/${node_arch}.complete"; \
    "${node_toolcache}/bin/node" --version; \
    chown -R runner:runner "${AGENT_TOOLSDIRECTORY}"; \
    rm -f "/tmp/${archive}" /tmp/node-hashes.sha256

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
# ~/.local and ~/.cache. The baked tool cache lives in the image's private
# writable layer rather than a shared volume, so setup-* installs cannot race.
RUN mkdir -p /home/runner/.cache /home/runner/.local/share/pnpm/store \
    && chown -R runner:runner /home/runner/.cache /home/runner/.local

# The inner dockerd cannot nest overlayfs on the container's own overlay FS.
VOLUME /var/lib/docker

COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
