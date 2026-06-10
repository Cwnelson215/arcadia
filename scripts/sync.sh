#!/usr/bin/env bash
# Sync the arcadia repo to bulbasaur for build/run.
#
# Capture + VA-API encode are hardware-specific to bulbasaur, so the code is
# edited on the workstation (source of truth = this git repo) but built and run
# on bulbasaur. This rsyncs over Tailscale; it does NOT push target/ or .git/.
#
# Usage (from the repo root on the workstation):
#   scripts/sync.sh
# then on bulbasaur:
#   ssh cwnelson@bulbasaur 'cd arcadia && cargo run -- --source test'
set -euo pipefail

HOST="${ARCADIA_HOST:-cwnelson@bulbasaur}"
DEST="${ARCADIA_DEST:-arcadia}"
SRC="$(cd "$(dirname "$0")/.." && pwd)/"

echo "rsync  $SRC  ->  $HOST:$DEST/"
rsync -az --delete \
  --exclude '.git/' \
  --exclude 'target/' \
  --exclude 'node_modules/' \
  "$SRC" "$HOST:$DEST/"

echo "done."
echo "build/run on bulbasaur:  ssh $HOST 'cd $DEST && cargo run -- --source test'"
