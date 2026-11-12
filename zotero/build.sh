#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

version=$(sed -n 's/^[[:space:]]*"version": "\([^"]*\)".*/\1/p' manifest.json | head -n 1)
if [[ -z "${version}" ]]; then
  echo "cannot read version from manifest.json" >&2
  exit 1
fi

binary_version=$(awk '
  /^\[package\]$/ { in_package = 1; next }
  /^\[/ { in_package = 0 }
  in_package && /^version[[:space:]]*=/ {
    sub(/^[^"]*"/, ""); sub(/".*$/, ""); print; exit
  }
' ../Cargo.toml)
if [[ -z "${binary_version}" ]]; then
  echo "cannot read package version from Cargo.toml" >&2
  exit 1
fi
if [[ "${version}" != "${binary_version}" ]]; then
  echo "Zotero version ${version} must match Rust binary version ${binary_version}; update manifest.json" >&2
  exit 1
fi

mkdir -p dist
xpi="dist/pdf-translate-zotero-${version}.xpi"
rm -f "${xpi}"

zip -r -X "${xpi}" \
  manifest.json \
  bootstrap.js \
  prefs.js \
  content \
  locale \
  -x '*.DS_Store' >/dev/null

echo "built ${xpi}"
