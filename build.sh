#!/bin/sh
# Builds the engine for Linux in Docker and puts it where the cookbook ships it from:
#   cookbook/depsolve/files/default/libdepsolve_rust.so
#
# The Rust tests run as part of the build. Set DEPSOLVE_PLATFORM=linux/arm64 for
# aarch64 servers (default linux/amd64).
set -eu
cd "$(dirname "$0")"

platform="${DEPSOLVE_PLATFORM:-linux/amd64}"
destination=cookbook/depsolve/files/default

docker build --platform "$platform" -f Dockerfile.build --output "type=local,dest=$destination" .
echo "built $destination/libdepsolve_rust.so for $platform"
shasum -a 256 "$destination/libdepsolve_rust.so" 2>/dev/null || sha256sum "$destination/libdepsolve_rust.so"
