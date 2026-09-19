#!/usr/bin/env bash
# Copy the C# SDK sources into the UPM package (sdk/upm/com.opussystems.api),
# which Unity projects reference by git URL. Unity does not follow symlinks,
# so the package carries a copy; CI runs this with --check so it never
# drifts from sdk/csharp.
set -euo pipefail
cd "$(dirname "$0")"
SRC=../csharp/OpusSystems.Api
DST=../upm/com.opussystems.api
if [[ "${1:-}" == "--check" ]]; then
  for f in "$SRC"/*.cs; do
    diff -q "$f" "$DST/$(basename "$f")" >/dev/null || { echo "out of date: $(basename "$f") — run sdk/unity/sync-sdk.sh" >&2; exit 1; }
  done
  echo "UPM package SDK copy is current"
  exit 0
fi
cp "$SRC"/*.cs "$DST"/
echo "synced $(ls "$SRC"/*.cs | wc -l | tr -d ' ') files into $DST"
