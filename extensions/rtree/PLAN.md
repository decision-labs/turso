# R-tree extension — SQLite parity plan & status

Tracking the port of SQLite's `ext/rtree/rtree.c` to `extensions/rtree`. Reference: a local
checkout of `sqlite/sqlite` at `../sqlite` (`ext/rtree/rtree.c`, `rtree1.test`…`rtreeJ.test`).

Status legend: ✅ done · 🟡 partial · ⛔ not started · 🚧 blocked

## Architecture notes

- rtree is a **loadable extension** (`liblimbo_rtree`), loaded via `.load`. It is *not* a core
  built-in like `generate_series`/`regexp` (those live in `core/` and register through
  `register_global_builtin_extensions`). Consequence: the `.sqltest` runner can't reach it yet
  (neither the rust-bindings backend nor the `tursodb` CLI backend has it linked), so regression
  coverage currently lives in `testing/cli_tests/extensions.py` (Python CLI, uses `.load`).
- Shadow tables `%_node` / `%_rowid` / `%_parent` use SQLite's on-disk layout. They are created
  lazily on first insert (SQLite creates them at `xCreate` time — a timing divergence, not a
  format one).

## Done

- ✅ `CREATE`/`USING rtree(...)` with up to 5 dimensions; honors user column names and `+aux` columns.
- ✅ xCreate argv aligned to SQLite `[module, db, table, ...USING-args]` (fixed core `VirtualTable::table`).
- ✅ xEof contract: engine consults `eof()` after `filter`/`next` (fixed core `ExtVirtualTable`).
- ✅ R*-tree split — `splitNodeStartree` (Beckmann 1990): per-axis sort, min margin → overlap → area.
- ✅ Insert: ChooseLeaf descent, leaf split, AdjustTree (MBR propagation up ancestors).
- ✅ Delete: underflow / `removeNode` + `reinsertNodeContent`, root single-child collapse, MBR tighten.
- ✅ Update: delete + reinsert, merging xUpdate NULL placeholders with the stored row.
- ✅ `best_index` / `filter`: rowid-lookup (idxNum 1) and range scan (idxNum 2); idx_str op/coord encoding.
- ✅ Implicit constraints (rtree-2): coord1≤coord2 per dim → ConstraintViolation; duplicate rowid rejected.
- ✅ Explicit rowid honored (xUpdate argv[1] threaded into `VTable::insert`); NULL → auto-assign.
- ✅ `DROP TABLE` drops shadow tables — xDestroy parity (Connection threaded into `VTable::destroy`).
- ✅ `rtreecheck` integrity walker as a pure-Rust method (`RtreeTable::integrity_check`).

## Remaining (rough priority order)

1. ⛔ **On-conflict** (`INSERT OR REPLACE`/`OR IGNORE`, rtree-12). A duplicate rowid always errors;
   the xUpdate conflict mode is not surfaced to the extension. Needs the FFI update path to pass
   `conflict_action` (already computed in `op_vupdate` emission) through to `VTable::insert`.
2. 🚧 **SQL-callable `rtreecheck()`** (function exists in Rust). Blocked: turso_ext scalar functions
   receive no `Connection`, so they can't query shadow tables. Needs a connection-aware scalar
   variant in turso_ext.
3. ⛔ **`rtree_i32`** — integer-coordinate module variant (`RTREE_COORD_INT32`). Needs a coord-type
   flag threaded through cell read/write and the area/margin/overlap math.
4. ⛔ **`sqlite_stat1` row estimates** in `best_index` (static `best_index` has no table handle).
5. ⛔ **MATCH operator + geometry callbacks** (`sqlite3_rtree_geometry_callback` /
   `sqlite3_rtree_query_callback`, op `0x46`, priority-queue best-first scan). Largest item.
6. ⛔ **geopoly** sibling module (separate file in SQLite; reuses shadow infra).
7. ⛔ **Make rtree a core built-in** — unblocks `.sqltest` coverage (CLI + bindings + sqltest runner).
   Either move into `core/` (like `series.rs`) or have `core` depend on `limbo_rtree`.

## Known limitations / intentional divergences

- Internal full nodes use **promote-to-internal** split (two new children under the split nodeno),
  not SQLite's `pLeft = pNode` + `rtreeInsertCell`-into-parent path.
- Constraint errors return a generic `ConstraintViolation` (engine reports "Constraint Violation"),
  not SQLite's exact text (`UNIQUE constraint failed: t.ii`, `rtree constraint failed: t.(x1<=x2)`).
- Auto-assign rowid uses an in-memory counter (high-water mark), not `MAX(rowid)` from `%_rowid`.
- Shadow tables created lazily on first insert rather than at `CREATE` time.

## Test coverage

- Rust unit tests (`extensions/rtree/src/lib.rs`): split heuristic, MBR math, cell geometry checks,
  constraint helpers, node/cell serialization (26 tests).
- Python CLI (`testing/cli_tests/extensions.py`): `test_rtree`, `test_rtree_aux`,
  `test_rtree_shadow_tables` (shadow tables materialize + dropped on DROP),
  `test_rtree_constraints` (auto/explicit rowid, duplicate + coord rejection),
  `test_rtree_delete_sequence` (1-d delete-to-empty incl. `%_rowid`).
- Ported from `rtree1.test`: rtree-1 (create/shadow/drop), rtree-2 (implicit constraints),
  rtree-5 (delete). Not yet ported: rtree-3 (scans), rtree-4 (insert), rtree-6 (update),
  rtree-7 (rename), rtree-8 (constrained scans), rtree-12 (on-conflict), rtree-14 (type coercion).
