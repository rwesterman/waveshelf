#!/usr/bin/env bash
# Runs scripts/package-linux.sh in an Ubuntu 22.04 container, from any host
# with Docker. Output lands in dist/.
#
# Usage: scripts/package-linux-docker.sh [x86_64|arm64]   (default: x86_64)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
case "${1:-x86_64}" in
  x86_64 | amd64) PLATFORM=linux/amd64 ;;
  arm64 | aarch64) PLATFORM=linux/arm64 ;;
  *) echo "unknown architecture: $1" >&2; exit 2 ;;
esac
IMAGE="waveshelf-linux-build:${PLATFORM#linux/}"

docker build --platform "$PLATFORM" -t "$IMAGE" "$ROOT/scripts/linux"
docker run --rm --platform "$PLATFORM" \
  --user "$(id -u):$(id -g)" \
  -v "$ROOT:/src" -w /src \
  "$IMAGE" scripts/package-linux.sh
