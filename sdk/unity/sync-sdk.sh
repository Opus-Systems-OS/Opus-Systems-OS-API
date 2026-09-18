#!/usr/bin/env bash
# Copy the C# SDK sources into the Unity sample. Unity does not follow
# symlinks reliably, so the sample carries a copy; CI runs this with
# --check to make sure the copy never drifts from sdk/csharp.
set -euo pipefail
cd "$(dirname "$0")"
SRC=../csharp/OpusSystems.Api
DST=JarvisSample/Assets/OpusSystems/Api
if [[ "${1:-}" == "--check" ]]; then
  for f in "$SRC"/*.cs; do
    diff -q "$f" "$DST/$(basename "$f")" >/dev/null || { echo "out of date: $(basename "$f") — run sdk/unity/sync-sdk.sh" >&2; exit 1; }
  done
  echo "unity sample SDK copy is current"
  exit 0
fi
cp "$SRC"/*.cs "$DST"/
echo "synced $(ls "$SRC"/*.cs | wc -l | tr -d ' ') files into $DST"
