#!/usr/bin/env bash
# Usage: collect-release.sh <artifacts-dir> <out-dir>
# Gathers the per-target tarballs that download-artifact leaves in <artifacts-dir>/<name>/
# and writes the checksums.txt that install.sh verifies against.
set -euo pipefail

src=${1:?usage: collect-release.sh <artifacts-dir> <out-dir>}
out=${2:?usage: collect-release.sh <artifacts-dir> <out-dir>}

mkdir -p "$out"
cp "$src"/*/*.tar.gz "$out"/
cd "$out"
sha256sum -- *.tar.gz >checksums.txt
