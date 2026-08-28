#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../images/runner"
docker build -t homerunner-runner:local .
