#!/usr/bin/env bash
# Compare SQLite (built-in rtree) vs tursodb + liblimbo_rtree for the same queries.
# Run from repo root after: cargo build --bin tursodb -p limbo_rtree
#
# Requires sqlite3 with SQLITE_ENABLE_RTREE (many distro builds include rtree).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXT="$ROOT/target/debug/liblimbo_rtree"
LABEL="rtree smoke"

REF_SQL="CREATE VIRTUAL TABLE rx USING rtree(id, xmin, xmax, ymin, ymax); INSERT INTO rx VALUES (1, 0.0, 10.0, 0.0, 10.0); SELECT id FROM rx WHERE xmin > -1.0 AND xmax < 15.0 AND ymin > -1.0 AND ymax < 15.0;"

TURSO_SQL=".load $EXT; $REF_SQL"

S=$(sqlite3 :memory: <<< ".mode list
$REF_SQL" 2>&1) || true
T=$(cd "$ROOT" && cargo run -q --bin tursodb -- :memory: "$TURSO_SQL" --output-mode list 2>&1) || true

if [[ "$S" == "$T" ]]; then
  echo "PASS: $LABEL"
else
  echo "FAIL: $LABEL"
  echo "  sqlite3: $(echo "$S" | head -20)"
  echo "  tursodb: $(echo "$T" | head -20)"
fi
