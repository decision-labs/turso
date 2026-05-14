//! R-Tree virtual table extension.
//!
//! A Rust implementation of SQLite's rtree extension for spatial indexing.
//! Uses the R*-tree split heuristic from Beckmann et al. 1990 (see [`split_node`]).
//!
//! ## Usage:
//!
//! ```sql
//! CREATE VIRTUAL TABLE my_rtree USING rtree(id, xmin, xmax, ymin, ymax);
//! INSERT INTO my_rtree VALUES(1, 0.0, 10.0, 0.0, 10.0);
//! SELECT * FROM my_rtree WHERE xmin > 5.0 AND xmax < 15.0;
//! ```
//!
//! Shadow tables follow SQLite's `ext/rtree/rtree.c`: `%_node`, `%_rowid`, `%_parent`.
//! Auxiliary columns (`+label` / `+label TEXT` in the column list) extend `%_rowid` after `nodeno`.
//!
//! ## SQLite parity (intentional gaps)
//!
//! - `MATCH` / `sqlite3_rtree_geometry_callback`-style geometry callbacks are not implemented.
//! - `RTREE_COORD_INT32` (32-bit integer coordinates) is not implemented.
//! - Row-count estimates from `sqlite_stat1` in `best_index` are not wired (static `best_index` has no table handle).
//! - Internal full nodes use the same **promote-to-internal** split as leaf insert (two new siblings under the split
//!   nodeno). SQLite's alternate `SplitNode` path (`pLeft = pNode`, then `rtreeInsertCell` into the parent for the
//!   right bbox, which may overflow the parent) is not implemented.
//! - Root collapse (`rtreeDeleteRowid` ~2978) queues cells at height `iDepth-1`; `descend_from_root_with_start` matches
//!   SQLite `ChooseLeaf` descent counts (`iDepth - iHeight`).
//!
//! ## Split algorithm
//!
//! [`split_node`] mirrors SQLite's `splitNodeStartree` (Beckmann et al. 1990): for each dimension, sort cells by
//! `(min, max)`, scan candidate split points `[MINCELLS..=N-MINCELLS]`, pick the axis with minimum total margin, and
//! within that axis pick the distribution with minimum overlap (tiebreak: total area). For tiny inputs
//! (`N < 2 * MINCELLS`) — only reachable from unit tests today — we fall back to a halving split.

use std::sync::Arc;
use turso_ext::{
    register_extension, Connection, ConstraintInfo, ConstraintOp, ConstraintUsage, IndexInfo,
    OrderByInfo, ResultCode, StepResult, VTabCursor, VTabKind, VTabModule, VTabModuleDerive,
    VTable, Value, ValueType,
};

register_extension! {
    vtabs: { RtreeModule }
}

const RTREE_MAX_DIMENSIONS: usize = 5;
/// Matches `RTREE_MAX_AUX_COLUMN` in SQLite `ext/rtree/rtree.c`.
const RTREE_MAX_AUX_COLUMN: usize = 100;
const RTREE_DEFAULT_ROWEST: i64 = 1048576;
/// Matches `RTREE_MIN_ROWEST` in SQLite `ext/rtree/rtree.c` (floor when estimating rows).
const RTREE_MIN_ROWEST: u32 = 100;

const RTREE_EQ: u8 = b'A';
const RTREE_LE: u8 = b'B';
const RTREE_LT: u8 = b'C';
const RTREE_GE: u8 = b'D';
const RTREE_GT: u8 = b'E';

const NOT_WITHIN: i32 = 0;
const PARTLY_WITHIN: i32 = 1;
const FULLY_WITHIN: i32 = 2;

const IDX_NUM_ROWID: i32 = 1;
const IDX_NUM_QUERY: i32 = 2;

const IDX_STR_ROWID: &str = "rowid_lookup";
const IDX_STR_QUERY: &str = "query";

/// Escape a column name for use inside SQLite `"identifier"` tokens.
fn quote_sql_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn value_to_owned(v: &Value) -> Value {
    match v.value_type() {
        ValueType::Null => Value::null(),
        ValueType::Integer => Value::from_integer(v.to_integer().unwrap_or(0)),
        ValueType::Float => Value::from_float(v.to_float().unwrap_or(0.0)),
        ValueType::Text => Value::from_text(v.to_text().unwrap_or("").to_string()),
        ValueType::Blob => Value::from_blob(v.to_blob().unwrap_or_default()),
        ValueType::Error => Value::null(),
    }
}

fn constraint_op_to_rtree_op(op: ConstraintOp) -> Option<u8> {
    match op {
        ConstraintOp::Eq => Some(RTREE_EQ),
        ConstraintOp::Le => Some(RTREE_LE),
        ConstraintOp::Lt => Some(RTREE_LT),
        ConstraintOp::Ge => Some(RTREE_GE),
        ConstraintOp::Gt => Some(RTREE_GT),
        _ => None,
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct RtreeModule;

impl RtreeModule {
    /// Coordinate columns first, then optional `+name` / `+name TYPE` auxiliary columns
    /// (stored on `%_rowid`, see SQLite `ext/rtree/rtree.c`).
    fn parse_column_args(args: &[Value]) -> Result<(usize, Vec<String>), ResultCode> {
        let mut n_dim2 = 0;
        let mut aux_names = Vec::new();
        let mut seen_aux = false;
        for arg in args {
            let Some(text) = arg.to_text() else {
                return Err(ResultCode::InvalidArgs);
            };
            if text.starts_with('+') {
                seen_aux = true;
                let rest = text[1..].trim();
                let name = rest
                    .split_whitespace()
                    .next()
                    .filter(|s| !s.is_empty())
                    .ok_or(ResultCode::InvalidArgs)?;
                aux_names.push(name.to_string());
            } else if !seen_aux {
                n_dim2 += 1;
            } else {
                break;
            }
        }
        if n_dim2 < 2 || n_dim2 > RTREE_MAX_DIMENSIONS * 2 || n_dim2 % 2 != 0 {
            return Err(ResultCode::InvalidArgs);
        }
        if aux_names.len() > RTREE_MAX_AUX_COLUMN {
            return Err(ResultCode::InvalidArgs);
        }
        Ok((n_dim2, aux_names))
    }
}

impl VTabModule for RtreeModule {
    type Table = RtreeTable;
    const VTAB_KIND: VTabKind = VTabKind::VirtualTable;
    const NAME: &'static str = "rtree";
    const READONLY: bool = false;

    fn create(args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        if args.len() < 5 {
            return Err(ResultCode::InvalidArgs);
        }

        let (n_dim2, aux_columns) = Self::parse_column_args(&args[4..])?;
        let n_dim = n_dim2 / 2;

        let n_bytes_per_cell: usize = 8 + n_dim2 * 4;
        let default_node_size: usize = 4096 - 64;

        let mut columns = String::from("id INTEGER PRIMARY KEY");
        for i in 0..n_dim {
            columns.push_str(", x");
            columns.push_str(&i.to_string());
            columns.push_str("min REAL");
            columns.push_str(", x");
            columns.push_str(&i.to_string());
            columns.push_str("max REAL");
        }
        for name in &aux_columns {
            columns.push_str(&format!(", {} TEXT", quote_sql_ident(name)));
        }
        let schema = format!("CREATE TABLE x ({})", columns);

        let table_name = args
            .get(2)
            .and_then(|v| v.to_text())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "x".to_string());

        let table = RtreeTable {
            n_dim2,
            n_bytes_per_cell,
            node_size: default_node_size,
            depth: 0,
            row_count: 0,
            node_count: 0,
            table_name,
            aux_columns,
        };

        Ok((schema, table))
    }
}

#[derive(Debug, Clone)]
struct RtreeTable {
    n_dim2: usize,
    n_bytes_per_cell: usize,
    node_size: usize,
    depth: usize,
    row_count: i64,
    node_count: i64,
    table_name: String,
    /// Auxiliary column names (`+col` in CREATE); persisted on `%_rowid` after `nodeno`.
    aux_columns: Vec<String>,
}

impl RtreeTable {
    fn max_cells(&self) -> usize {
        (self.node_size - 4) / self.n_bytes_per_cell
    }

    /// Matches `RTREE_MINCELLS` in SQLite `ext/rtree/rtree.c`.
    fn min_cells(&self) -> usize {
        self.max_cells() / 3
    }

    fn shadow_node_table(&self) -> String {
        format!("{}_node", self.table_name)
    }

    fn shadow_rowid_table(&self) -> String {
        format!("{}_rowid", self.table_name)
    }

    fn shadow_parent_table(&self) -> String {
        format!("{}_parent", self.table_name)
    }

    fn load_root_node(&self, conn: &Arc<Connection>) -> Result<RtreeNode, ResultCode> {
        let sql = format!(
            "SELECT data FROM {} WHERE nodeno = 1",
            self.shadow_node_table()
        );
        let mut stmt = conn.prepare(&sql).map_err(|_| ResultCode::Error)?;

        if stmt.step() == StepResult::Row {
            let row = stmt.get_row();
            if let Some(blob_val) = row.first() {
                if let Some(blob) = blob_val.blob_ref() {
                    let depth = u16::from_be_bytes([blob[0], blob[1]]) as usize;
                    let mut data_vec = vec![0u8; self.node_size];
                    let copy_len = blob.len().min(self.node_size);
                    data_vec[..copy_len].copy_from_slice(&blob[..copy_len]);
                    return Ok(RtreeNode {
                        node_no: 1,
                        depth,
                        data: data_vec,
                        is_dirty: false,
                    });
                }
            }
        }

        Ok(RtreeNode::new(1, 0, self.node_size))
    }

    fn create_shadow_tables(&mut self, conn: &Arc<Connection>) -> Result<(), ResultCode> {
        let node_sql = format!(
            "CREATE TABLE {} (nodeno INTEGER PRIMARY KEY, data BLOB)",
            self.shadow_node_table()
        );
        conn.execute(&node_sql, &[])
            .map_err(|_| ResultCode::Error)?;

        let mut rowid_sql = format!(
            "CREATE TABLE {} (rowid INTEGER PRIMARY KEY, nodeno INTEGER",
            self.shadow_rowid_table()
        );
        for name in &self.aux_columns {
            rowid_sql.push_str(&format!(", {} TEXT", quote_sql_ident(name)));
        }
        rowid_sql.push(')');
        conn.execute(&rowid_sql, &[])
            .map_err(|_| ResultCode::Error)?;

        let parent_sql = format!(
            "CREATE TABLE {} (nodeno INTEGER PRIMARY KEY, parentnode INTEGER)",
            self.shadow_parent_table()
        );
        conn.execute(&parent_sql, &[])
            .map_err(|_| ResultCode::Error)?;

        let mut root_node = RtreeNode::new(1, 0, self.node_size);
        root_node.set_depth(0);
        self.write_node(conn, 1, &root_node)?;

        self.node_count = 1;
        self.depth = 0;
        Ok(())
    }

    fn read_node(
        &self,
        conn: &Arc<Connection>,
        node_no: i64,
    ) -> Result<Option<RtreeNode>, ResultCode> {
        if node_no == 0 {
            return Ok(None);
        }

        let sql = format!(
            "SELECT data FROM {} WHERE nodeno = ?",
            self.shadow_node_table()
        );
        let mut stmt = conn.prepare(&sql).map_err(|_| ResultCode::Error)?;

        stmt.bind_at(
            std::num::NonZeroUsize::new(1).unwrap(),
            Value::from_integer(node_no),
        );

        if stmt.step() == StepResult::Row {
            let row = stmt.get_row();
            if let Some(blob_val) = row.first() {
                if let Some(blob) = blob_val.blob_ref() {
                    let depth = if node_no == 1 {
                        u16::from_be_bytes([blob[0], blob[1]]) as usize
                    } else {
                        0
                    };
                    let mut data_vec = vec![0u8; self.node_size];
                    let copy_len = blob.len().min(self.node_size);
                    data_vec[..copy_len].copy_from_slice(&blob[..copy_len]);
                    return Ok(Some(RtreeNode {
                        node_no,
                        depth,
                        data: data_vec,
                        is_dirty: false,
                    }));
                }
            }
        }

        Ok(None)
    }

    fn write_node(
        &self,
        conn: &Arc<Connection>,
        node_no: i64,
        node: &RtreeNode,
    ) -> Result<(), ResultCode> {
        let sql = format!(
            "INSERT OR REPLACE INTO {} (nodeno, data) VALUES (?, ?)",
            self.shadow_node_table()
        );
        let data_slice = node.data.clone();
        conn.execute(
            &sql,
            &[Value::from_integer(node_no), Value::from_blob(data_slice)],
        )
        .map_err(|_| ResultCode::Error)?;
        Ok(())
    }

    fn write_rowid_map(
        &self,
        conn: &Arc<Connection>,
        rowid: i64,
        nodeno: i64,
        aux: &[Value],
    ) -> Result<(), ResultCode> {
        if aux.len() != self.aux_columns.len() {
            return Err(ResultCode::InvalidArgs);
        }
        let mut sql = format!(
            "INSERT OR REPLACE INTO {} (rowid, nodeno",
            self.shadow_rowid_table()
        );
        for name in &self.aux_columns {
            sql.push_str(&format!(", {}", quote_sql_ident(name)));
        }
        sql.push_str(") VALUES (?");
        sql.push_str(", ?");
        for _ in 0..self.aux_columns.len() {
            sql.push_str(", ?");
        }
        sql.push(')');
        let mut params: Vec<Value> = vec![Value::from_integer(rowid), Value::from_integer(nodeno)];
        params.extend(aux.iter().map(value_to_owned));
        conn.execute(&sql, &params).map_err(|_| ResultCode::Error)?;
        Ok(())
    }

    fn get_rowid_nodeno(
        &self,
        conn: &Arc<Connection>,
        rowid: i64,
    ) -> Result<Option<i64>, ResultCode> {
        let sql = format!(
            "SELECT nodeno FROM {} WHERE rowid = ?",
            self.shadow_rowid_table()
        );
        let mut stmt = conn.prepare(&sql).map_err(|_| ResultCode::Error)?;

        stmt.bind_at(
            std::num::NonZeroUsize::new(1).unwrap(),
            Value::from_integer(rowid),
        );

        if stmt.step() == StepResult::Row {
            let row = stmt.get_row();
            if let Some(val) = row.first() {
                return Ok(val.to_integer());
            }
        }
        Ok(None)
    }

    fn delete_rowid_map(&self, conn: &Arc<Connection>, rowid: i64) -> Result<(), ResultCode> {
        let sql = format!("DELETE FROM {} WHERE rowid = ?", self.shadow_rowid_table());
        conn.execute(&sql, &[Value::from_integer(rowid)])
            .map_err(|_| ResultCode::Error)?;
        Ok(())
    }

    /// Leaf [`RtreeCell`] for `rowid` (geometry only; aux lives on `%_rowid`).
    fn read_leaf_cell(&self, conn: &Arc<Connection>, rowid: i64) -> Result<RtreeCell, ResultCode> {
        let Some(nodeno) = self.get_rowid_nodeno(conn, rowid)? else {
            return Err(ResultCode::NotFound);
        };
        let Some(node) = self.read_node(conn, nodeno)? else {
            return Err(ResultCode::Corrupt);
        };
        let idx = self
            .find_cell_index(&node, rowid)
            .ok_or(ResultCode::NotFound)?;
        Ok(node.get_cell(self.n_dim2, self.n_bytes_per_cell, idx))
    }

    /// SQLite `xUpdate` supplies NULL in `argv[2..]` for unchanged columns; merge with the stored row
    /// before delete+reinsert.
    fn merge_xupdate_argv_with_existing_row(
        &self,
        conn: &Arc<Connection>,
        old_rowid: i64,
        args: &[Value],
    ) -> Result<Vec<Value>, ResultCode> {
        let required = self.n_dim2 + 1 + self.aux_columns.len();
        if args.len() < required {
            return Err(ResultCode::InvalidArgs);
        }
        let old_cell = self.read_leaf_cell(conn, old_rowid)?;
        let old_aux = self.read_rowid_aux_values(conn, old_rowid)?;

        let mut out = Vec::with_capacity(required);

        let pk = if matches!(args[0].value_type(), ValueType::Null) {
            Value::from_integer(old_rowid)
        } else if let Some(id) = args[0].to_integer() {
            Value::from_integer(id)
        } else {
            return Err(ResultCode::InvalidArgs);
        };
        out.push(pk);

        for i in 0..self.n_dim2 {
            let v = &args[i + 1];
            let merged = if matches!(v.value_type(), ValueType::Null) {
                Value::from_float(f64::from(old_cell.coords[i]))
            } else if let Some(f) = v.to_float() {
                Value::from_float(f)
            } else {
                return Err(ResultCode::InvalidArgs);
            };
            out.push(merged);
        }

        for j in 0..self.aux_columns.len() {
            let v = &args[1 + self.n_dim2 + j];
            let merged = if matches!(v.value_type(), ValueType::Null) {
                value_to_owned(&old_aux[j])
            } else {
                value_to_owned(v)
            };
            out.push(merged);
        }

        Ok(out)
    }

    fn delete_shadow_node_row(
        &self,
        conn: &Arc<Connection>,
        nodeno: i64,
    ) -> Result<(), ResultCode> {
        let sql = format!("DELETE FROM {} WHERE nodeno = ?", self.shadow_node_table());
        conn.execute(&sql, &[Value::from_integer(nodeno)])
            .map_err(|_| ResultCode::Error)?;
        Ok(())
    }

    fn delete_shadow_parent_row(
        &self,
        conn: &Arc<Connection>,
        nodeno: i64,
    ) -> Result<(), ResultCode> {
        let sql = format!(
            "DELETE FROM {} WHERE nodeno = ?",
            self.shadow_parent_table()
        );
        conn.execute(&sql, &[Value::from_integer(nodeno)])
            .map_err(|_| ResultCode::Error)?;
        Ok(())
    }

    fn read_rowid_aux_values(
        &self,
        conn: &Arc<Connection>,
        rowid: i64,
    ) -> Result<Vec<Value>, ResultCode> {
        if self.aux_columns.is_empty() {
            return Ok(Vec::new());
        }
        let cols = self
            .aux_columns
            .iter()
            .map(|n| quote_sql_ident(n))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {} FROM {} WHERE rowid = ?",
            cols,
            self.shadow_rowid_table()
        );
        let mut stmt = conn.prepare(&sql).map_err(|_| ResultCode::Error)?;
        stmt.bind_at(
            std::num::NonZeroUsize::new(1).unwrap(),
            Value::from_integer(rowid),
        );
        if stmt.step() == StepResult::Row {
            let row = stmt.get_row();
            if row.len() < self.aux_columns.len() {
                return Err(ResultCode::Corrupt);
            }
            let mut out = Vec::with_capacity(self.aux_columns.len());
            for i in 0..self.aux_columns.len() {
                out.push(value_to_owned(&row[i]));
            }
            return Ok(out);
        }
        Err(ResultCode::NotFound)
    }

    /// Insert one leaf row (coordinates + aux) at `rowid`, matching the body of [`VTable::insert`].
    fn insert_leaf_row(
        &mut self,
        conn: &Arc<Connection>,
        rowid: i64,
        cell: &RtreeCell,
        aux: &[Value],
    ) -> Result<(), ResultCode> {
        if aux.len() != self.aux_columns.len() {
            return Err(ResultCode::InvalidArgs);
        }
        self.row_count = self.row_count.max(rowid);

        if self.node_count == 0 {
            self.create_shadow_tables(conn)?;
        }

        let leaf_opt = choose_leaf(self, Some(conn), cell, self.n_dim2);
        let leaf_nodeno = if let Some(mut leaf) = leaf_opt {
            let n_cells = leaf.cell_count();
            let max_cells = self.max_cells();
            let nodeno = leaf.node_no;

            if n_cells < max_cells {
                leaf.set_cell(self.n_dim2, self.n_bytes_per_cell, n_cells, cell);
                leaf.set_cell_count(n_cells + 1);
                self.write_node(conn, nodeno, &leaf)?;
                nodeno
            } else {
                let (left, right, new_in_right) = split_node(self, &leaf, cell, self.n_dim2);
                self.node_count += 2;
                let left_no = self.node_count - 1;
                let right_no = self.node_count;
                self.write_node(conn, left_no, &left)?;
                self.write_node(conn, right_no, &right)?;
                self.write_parent_map(conn, left_no, nodeno)?;
                self.write_parent_map(conn, right_no, nodeno)?;

                let cell_left = bounding_cell_for_child_node(self, &left, left_no, self.n_dim2);
                let cell_right = bounding_cell_for_child_node(self, &right, right_no, self.n_dim2);
                leaf.set_cell(self.n_dim2, self.n_bytes_per_cell, 0, &cell_left);
                leaf.set_cell(self.n_dim2, self.n_bytes_per_cell, 1, &cell_right);
                leaf.set_cell_count(2);
                if nodeno == 1 {
                    leaf.set_depth(leaf.tree_depth() + 1);
                }
                self.write_node(conn, nodeno, &leaf)?;
                if new_in_right {
                    right_no
                } else {
                    left_no
                }
            }
        } else {
            self.node_count += 1;
            let new_node_no = self.node_count;
            let mut new_node = RtreeNode::new(new_node_no, 0, self.node_size);
            new_node.set_cell(self.n_dim2, self.n_bytes_per_cell, 0, cell);
            new_node.set_cell_count(1);
            self.write_node(conn, new_node_no, &new_node)?;
            self.depth = 1;
            new_node_no
        };

        self.adjust_ancestry_mbr(conn, leaf_nodeno)?;
        self.write_rowid_map(conn, rowid, leaf_nodeno, aux)?;
        Ok(())
    }

    /// Split a **full** internal node by promoting it like [`insert_leaf_row`] does for leaves: allocate two child
    /// nodes, move cells, reparent subtree roots in `%_parent`, then store two bounding cells in `split_nodeno`.
    fn split_internal_node_and_promote(
        &mut self,
        conn: &Arc<Connection>,
        split_nodeno: i64,
        new_cell: &RtreeCell,
    ) -> Result<(), ResultCode> {
        let Some(target) = self.read_node(conn, split_nodeno)? else {
            return Err(ResultCode::Corrupt);
        };
        let (left, right, _new_in_right) = split_node(self, &target, new_cell, self.n_dim2);
        self.node_count += 2;
        let left_no = self.node_count - 1;
        let right_no = self.node_count;

        self.write_node(conn, left_no, &left)?;
        self.write_node(conn, right_no, &right)?;

        for i in 0..left.cell_count() {
            let c = left.get_cell(self.n_dim2, self.n_bytes_per_cell, i);
            self.write_parent_map(conn, c.rowid, left_no)?;
        }
        for i in 0..right.cell_count() {
            let c = right.get_cell(self.n_dim2, self.n_bytes_per_cell, i);
            self.write_parent_map(conn, c.rowid, right_no)?;
        }

        self.write_parent_map(conn, left_no, split_nodeno)?;
        self.write_parent_map(conn, right_no, split_nodeno)?;

        let cell_left = bounding_cell_for_child_node(self, &left, left_no, self.n_dim2);
        let cell_right = bounding_cell_for_child_node(self, &right, right_no, self.n_dim2);

        let mut internal = target;
        internal.set_cell(self.n_dim2, self.n_bytes_per_cell, 0, &cell_left);
        internal.set_cell(self.n_dim2, self.n_bytes_per_cell, 1, &cell_right);
        internal.set_cell_count(2);
        if split_nodeno == 1 {
            internal.set_depth(internal.tree_depth() + 1);
        }
        self.write_node(conn, split_nodeno, &internal)?;
        self.adjust_ancestry_mbr(conn, split_nodeno)?;
        Ok(())
    }

    /// Reinsert one internal pointer cell (`rtreeInsertCell` with `iHeight > 0` in `rtree.c`).
    fn insert_internal_cell_at_height(
        &mut self,
        conn: &Arc<Connection>,
        cell: &RtreeCell,
        cell_height: usize,
    ) -> Result<(), ResultCode> {
        let root = self.load_root_node(conn)?;
        let sqlite_depth = root.tree_depth().saturating_sub(1);
        let iterations = sqlite_depth.saturating_sub(cell_height);
        let Some(mut target) =
            descend_from_root_with_start(self, Some(conn), root, cell, self.n_dim2, iterations)
        else {
            return Err(ResultCode::Corrupt);
        };
        let nodeno = target.node_no;
        let n = target.cell_count();
        if n >= self.max_cells() {
            return self.split_internal_node_and_promote(conn, nodeno, cell);
        }
        target.set_cell(self.n_dim2, self.n_bytes_per_cell, n, cell);
        target.set_cell_count(n + 1);
        self.write_node(conn, nodeno, &target)?;
        self.adjust_ancestry_mbr(conn, nodeno)?;
        self.write_parent_map(conn, cell.rowid, nodeno)?;
        Ok(())
    }

    /// After removing a cell, enforce SQLite-style minimum fill (`RTREE_MINCELLS`): detach underfull **leaf or internal**
    /// nodes, then queue their cells for reinsert at `height` (`removeNode` / `reinsertNodeContent` in `rtree.c`).
    fn fix_after_cell_removal(
        &mut self,
        conn: &Arc<Connection>,
        nodeno: i64,
        height: usize,
        pending: &mut Vec<(Vec<RtreeCell>, usize)>,
        depth_guard: &mut usize,
    ) -> Result<(), ResultCode> {
        *depth_guard += 1;
        if *depth_guard > 500 {
            return Err(ResultCode::Corrupt);
        }

        let min_c = self.min_cells();
        let Some(mut node) = self.read_node(conn, nodeno)? else {
            return Ok(());
        };
        let n = node.cell_count();

        if nodeno == 1 && n == 0 {
            node.set_cell_count(0);
            node.set_depth(0);
            self.write_node(conn, 1, &node)?;
            self.depth = 0;
            return Ok(());
        }

        if n >= min_c {
            if n > 0 {
                self.tighten_ancestry_mbr(conn, nodeno)?;
            }
            return Ok(());
        }

        if nodeno == 1 {
            return Ok(());
        }

        let cells: Vec<RtreeCell> = (0..n)
            .map(|i| node.get_cell(self.n_dim2, self.n_bytes_per_cell, i))
            .collect();

        let parent_no = self
            .get_parent_nodeno(conn, nodeno)?
            .ok_or(ResultCode::Corrupt)?;
        let mut parent = self
            .read_node(conn, parent_no)?
            .ok_or(ResultCode::Corrupt)?;
        let idx = self
            .find_cell_index(&parent, nodeno)
            .ok_or(ResultCode::Corrupt)?;
        self.delete_cell_from_node(&mut parent, idx);
        self.write_node(conn, parent_no, &parent)?;

        self.fix_after_cell_removal(conn, parent_no, height + 1, pending, depth_guard)?;

        self.delete_shadow_node_row(conn, nodeno)?;
        self.delete_shadow_parent_row(conn, nodeno)?;

        pending.push((cells, height));
        Ok(())
    }

    /// If the root has exactly one subtree after deletes (`rtreeDeleteRowid` in `rtree.c`, ~2978–3000), detach that
    /// child, drop its shadow rows, lower the stored depth, and queue its cells for reinsert at height `iDepth-1`.
    fn maybe_collapse_root_single_child(
        &mut self,
        conn: &Arc<Connection>,
        pending: &mut Vec<(Vec<RtreeCell>, usize)>,
    ) -> Result<(), ResultCode> {
        let Some(mut root) = self.read_node(conn, 1)? else {
            return Ok(());
        };
        let r = root.tree_depth();
        // Align with SQLite `iDepth > 0 && NCELL(pRoot)==1`: estimate SQLite depth as r - 1 (see ChooseLeaf descent).
        let sqlite_depth = r.saturating_sub(1);
        if sqlite_depth == 0 || root.cell_count() != 1 {
            return Ok(());
        }

        let child_no = root.get_cell(self.n_dim2, self.n_bytes_per_cell, 0).rowid;
        let Some(child) = self.read_node(conn, child_no)? else {
            return Err(ResultCode::Corrupt);
        };

        let n = child.cell_count();
        let cells: Vec<RtreeCell> = (0..n)
            .map(|i| child.get_cell(self.n_dim2, self.n_bytes_per_cell, i))
            .collect();

        root.set_cell_count(0);
        // After collapse SQLite sets `iDepth--`. With root blob depth `R ≈ iDepth + 1`, new `R' = iDepth` (pre-collapse).
        root.set_depth(sqlite_depth);
        self.write_node(conn, 1, &root)?;
        self.depth = root.tree_depth();

        self.delete_shadow_node_row(conn, child_no)?;
        self.delete_shadow_parent_row(conn, child_no)?;

        let reinsert_height = sqlite_depth.saturating_sub(1);
        pending.push((cells, reinsert_height));
        Ok(())
    }

    fn write_parent_map(
        &self,
        conn: &Arc<Connection>,
        nodeno: i64,
        parentnode: i64,
    ) -> Result<(), ResultCode> {
        let sql = format!(
            "INSERT OR REPLACE INTO {} (nodeno, parentnode) VALUES (?, ?)",
            self.shadow_parent_table()
        );
        conn.execute(
            &sql,
            &[Value::from_integer(nodeno), Value::from_integer(parentnode)],
        )
        .map_err(|_| ResultCode::Error)?;
        Ok(())
    }

    fn get_parent_nodeno(
        &self,
        conn: &Arc<Connection>,
        nodeno: i64,
    ) -> Result<Option<i64>, ResultCode> {
        if nodeno <= 1 {
            return Ok(None);
        }
        let sql = format!(
            "SELECT parentnode FROM {} WHERE nodeno = ?",
            self.shadow_parent_table()
        );
        let mut stmt = conn.prepare(&sql).map_err(|_| ResultCode::Error)?;
        stmt.bind_at(
            std::num::NonZeroUsize::new(1).unwrap(),
            Value::from_integer(nodeno),
        );
        if stmt.step() == StepResult::Row {
            let row = stmt.get_row();
            if let Some(val) = row.first() {
                return Ok(val.to_integer());
            }
        }
        Ok(None)
    }

    /// Walk `%_parent` toward the root and expand each ancestor cell so it covers the child's
    /// tight MBR (SQLite `AdjustTree` in `ext/rtree/rtree.c`).
    ///
    /// Note: SQLite also splits **internal** nodes when `nodeInsertCell` overflows (`SplitNode`);
    /// our insert path keeps parent fanout stable by converting full leaves in place; internal
    /// splits would be needed if we inserted sibling pointers into parents without that invariant.
    fn adjust_ancestry_mbr(
        &self,
        conn: &Arc<Connection>,
        mut nodeno: i64,
    ) -> Result<(), ResultCode> {
        let mut hops = 0;
        loop {
            if hops > 100 {
                return Err(ResultCode::Error);
            }
            hops += 1;

            let Some(node) = self.read_node(conn, nodeno)? else {
                break;
            };
            let Some(agg) = union_mbr_of_node_cells(self, &node, self.n_dim2) else {
                break;
            };
            let Some(parent_no) = self.get_parent_nodeno(conn, nodeno)? else {
                break;
            };
            let Some(mut parent_node) = self.read_node(conn, parent_no)? else {
                return Err(ResultCode::Error);
            };
            let Some(idx) = self.find_cell_index(&parent_node, nodeno) else {
                return Err(ResultCode::Error);
            };
            let mut pc = parent_node.get_cell(self.n_dim2, self.n_bytes_per_cell, idx);
            if !cell_contains_mbr(&pc, &agg, self.n_dim2) {
                let merged = cell_union_mbr_coords(&pc, &agg, self.n_dim2);
                pc.coords = merged.coords;
                parent_node.set_cell(self.n_dim2, self.n_bytes_per_cell, idx, &pc);
                self.write_node(conn, parent_no, &parent_node)?;
            }
            nodeno = parent_no;
        }
        Ok(())
    }

    /// Walk toward the root and **replace** each ancestor pointer cell's MBR with the tight union of the
    /// child node's cells (SQLite `fixBoundingBox` / tightening after delete).
    fn tighten_ancestry_mbr(
        &self,
        conn: &Arc<Connection>,
        mut nodeno: i64,
    ) -> Result<(), ResultCode> {
        let mut hops = 0;
        loop {
            if hops > 100 {
                return Err(ResultCode::Error);
            }
            hops += 1;

            let Some(node) = self.read_node(conn, nodeno)? else {
                break;
            };
            if node.cell_count() == 0 {
                break;
            }
            let Some(tight) = union_mbr_of_node_cells(self, &node, self.n_dim2) else {
                break;
            };
            let Some(parent_no) = self.get_parent_nodeno(conn, nodeno)? else {
                break;
            };
            let Some(mut parent_node) = self.read_node(conn, parent_no)? else {
                return Err(ResultCode::Error);
            };
            let Some(idx) = self.find_cell_index(&parent_node, nodeno) else {
                return Err(ResultCode::Error);
            };
            let mut pc = parent_node.get_cell(self.n_dim2, self.n_bytes_per_cell, idx);
            let ptr = pc.rowid;
            pc.coords = tight.coords;
            pc.rowid = ptr;
            parent_node.set_cell(self.n_dim2, self.n_bytes_per_cell, idx, &pc);
            self.write_node(conn, parent_no, &parent_node)?;
            nodeno = parent_no;
        }
        Ok(())
    }

    fn find_cell_index(&self, node: &RtreeNode, rowid: i64) -> Option<usize> {
        let n_cells = node.cell_count();
        for i in 0..n_cells {
            let cell = node.get_cell(self.n_dim2, self.n_bytes_per_cell, i);
            if cell.rowid == rowid {
                return Some(i);
            }
        }
        None
    }

    fn delete_cell_from_node(&self, node: &mut RtreeNode, cell_index: usize) {
        let n_cells = node.cell_count();
        if cell_index >= n_cells {
            return;
        }

        let cell_size = self.n_bytes_per_cell;
        let start_offset = 4 + cell_size * cell_index;
        let end_offset = 4 + cell_size * (cell_index + 1);

        if end_offset < node.data.len() {
            node.data.copy_within(end_offset.., start_offset);
        }

        node.set_cell_count(n_cells - 1);
        node.is_dirty = true;
    }
}

#[derive(Debug, Clone)]
struct RtreeNode {
    node_no: i64,
    depth: usize,
    data: Vec<u8>,
    is_dirty: bool,
}

impl RtreeNode {
    fn new(node_no: i64, depth: usize, size: usize) -> Self {
        let mut data = vec![0u8; size];
        if node_no == 1 {
            data[0] = (depth >> 8) as u8;
            data[1] = (depth & 0xff) as u8;
        }
        RtreeNode {
            node_no,
            depth,
            data,
            is_dirty: true,
        }
    }

    fn cell_count(&self) -> usize {
        u16::from_be_bytes([self.data[2], self.data[3]]) as usize
    }

    fn set_cell_count(&mut self, count: usize) {
        self.data[2] = (count >> 8) as u8;
        self.data[3] = (count & 0xff) as u8;
        self.is_dirty = true;
    }

    fn tree_depth(&self) -> usize {
        if self.node_no == 1 {
            u16::from_be_bytes([self.data[0], self.data[1]]) as usize
        } else {
            0
        }
    }

    fn set_depth(&mut self, depth: usize) {
        if self.node_no == 1 {
            self.data[0] = (depth >> 8) as u8;
            self.data[1] = (depth & 0xff) as u8;
            self.is_dirty = true;
        }
    }

    fn read_coord(&self, offset: usize) -> f32 {
        let bytes = [
            self.data[offset],
            self.data[offset + 1],
            self.data[offset + 2],
            self.data[offset + 3],
        ];
        f32::from_le_bytes(bytes)
    }

    fn write_coord(&mut self, offset: usize, coord: f32) {
        let bytes = coord.to_le_bytes();
        self.data[offset..offset + 4].copy_from_slice(&bytes);
        self.is_dirty = true;
    }

    fn read_int64(&self, offset: usize) -> i64 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.data[offset..offset + 8]);
        i64::from_le_bytes(bytes)
    }

    fn write_int64(&mut self, offset: usize, value: i64) {
        let bytes = value.to_le_bytes();
        self.data[offset..offset + 8].copy_from_slice(&bytes);
        self.is_dirty = true;
    }

    fn get_cell(&self, n_dim2: usize, n_bytes_per_cell: usize, i_cell: usize) -> RtreeCell {
        let offset = 4 + n_bytes_per_cell * i_cell;
        let rowid = self.read_int64(offset);
        let mut cell = RtreeCell::new(rowid);
        let coord_offset = offset + 8;
        for i in 0..n_dim2 {
            cell.coords[i] = self.read_coord(coord_offset + i * 4);
        }
        cell
    }

    fn set_cell(
        &mut self,
        n_dim2: usize,
        n_bytes_per_cell: usize,
        i_cell: usize,
        cell: &RtreeCell,
    ) {
        let offset = 4 + n_bytes_per_cell * i_cell;
        self.write_int64(offset, cell.rowid);
        let coord_offset = offset + 8;
        for i in 0..n_dim2 {
            self.write_coord(coord_offset + i * 4, cell.coords[i]);
        }
    }

    fn coords(
        &self,
        n_dim2: usize,
        n_bytes_per_cell: usize,
        i_cell: usize,
    ) -> [f32; RTREE_MAX_DIMENSIONS * 2] {
        let offset = 4 + n_bytes_per_cell * i_cell + 8;
        let mut coords = [0.0; RTREE_MAX_DIMENSIONS * 2];
        for i in 0..n_dim2 {
            coords[i] = self.read_coord(offset + i * 4);
        }
        coords
    }
}

#[derive(Debug, Clone)]
struct RtreeCell {
    rowid: i64,
    coords: [f32; RTREE_MAX_DIMENSIONS * 2],
}

impl RtreeCell {
    fn new(rowid: i64) -> Self {
        RtreeCell {
            rowid,
            coords: [0.0; RTREE_MAX_DIMENSIONS * 2],
        }
    }
}

#[derive(Debug)]
struct RtreeCursor {
    at_eof: bool,
    rowid: i64,
    current_coords: [f32; RTREE_MAX_DIMENSIONS * 2],
    constraints: Vec<RtreeConstraint>,
    conn: Option<Arc<Connection>>,
    n_dim2: usize,
    n_bytes_per_cell: usize,
    node_size: usize,
    table_name: String,
    aux_columns: Vec<String>,
    aux_values: Vec<Value>,
}

impl RtreeCursor {
    fn new(
        conn: Option<Arc<Connection>>,
        n_dim2: usize,
        n_bytes_per_cell: usize,
        node_size: usize,
        table_name: String,
        aux_columns: Vec<String>,
    ) -> Self {
        RtreeCursor {
            at_eof: true,
            rowid: 0,
            current_coords: [0.0; RTREE_MAX_DIMENSIONS * 2],
            constraints: Vec::new(),
            conn,
            n_dim2,
            n_bytes_per_cell,
            node_size,
            table_name,
            aux_columns,
            aux_values: Vec::new(),
        }
    }

    fn shadow_rowid_sql_name(&self) -> String {
        format!("{}_rowid", self.table_name)
    }

    fn load_aux_values(&mut self) -> Result<(), ResultCode> {
        self.aux_values.clear();
        if self.aux_columns.is_empty() || self.rowid == 0 {
            return Ok(());
        }
        let conn = self.conn.as_ref().ok_or(ResultCode::Error)?;
        let col_list = self
            .aux_columns
            .iter()
            .map(|n| quote_sql_ident(n))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {} FROM {} WHERE rowid = ?",
            col_list,
            self.shadow_rowid_sql_name()
        );
        let mut stmt = conn.prepare(&sql).map_err(|_| ResultCode::Error)?;
        stmt.bind_at(
            std::num::NonZeroUsize::new(1).unwrap(),
            Value::from_integer(self.rowid),
        );
        if stmt.step() == StepResult::Row {
            let row = stmt.get_row();
            self.aux_values = row.iter().map(value_to_owned).collect();
        }
        Ok(())
    }

    fn load_node(&self, node_no: i64) -> Option<RtreeNode> {
        let conn = self.conn.as_ref()?;
        let shadow_node_table = format!("{}_node", self.table_name);
        let sql = format!("SELECT data FROM {} WHERE nodeno = ?", shadow_node_table);
        let mut stmt = conn.prepare(&sql).ok()?;
        stmt.bind_at(
            std::num::NonZeroUsize::new(1).unwrap(),
            Value::from_integer(node_no),
        );
        if stmt.step() == StepResult::Row {
            let row = stmt.get_row();
            if let Some(blob_val) = row.first() {
                if let Some(blob) = blob_val.blob_ref() {
                    let depth = if node_no == 1 {
                        u16::from_be_bytes([blob[0], blob[1]]) as usize
                    } else {
                        0
                    };
                    let mut data_vec = vec![0u8; self.node_size];
                    let copy_len = blob.len().min(self.node_size);
                    data_vec[..copy_len].copy_from_slice(&blob[..copy_len]);
                    return Some(RtreeNode {
                        node_no,
                        depth,
                        data: data_vec,
                        is_dirty: false,
                    });
                }
            }
        }
        None
    }
}

#[derive(Debug)]
struct RtreeConstraint {
    i_coord: usize,
    op: u8,
    value: f64,
}

fn leaf_constraint(constraint: &RtreeConstraint, cell: &RtreeCell, n_dim2: usize) -> i32 {
    let coord_idx = constraint.i_coord;
    if coord_idx >= n_dim2 * 2 {
        return FULLY_WITHIN;
    }

    let coord_min = if coord_idx < n_dim2 {
        cell.coords[coord_idx]
    } else {
        cell.coords[n_dim2 + (coord_idx % n_dim2)]
    };
    let coord_max = if coord_idx < n_dim2 {
        cell.coords[n_dim2 + coord_idx]
    } else {
        cell.coords[coord_idx % n_dim2]
    };

    let val = constraint.value as f32;
    match constraint.op {
        b'A' => {
            if val != coord_min && val != coord_max {
                NOT_WITHIN
            } else {
                FULLY_WITHIN
            }
        }
        b'B' => {
            if val < coord_min {
                NOT_WITHIN
            } else if val > coord_max {
                PARTLY_WITHIN
            } else {
                FULLY_WITHIN
            }
        }
        b'C' => {
            if val <= coord_max {
                NOT_WITHIN
            } else {
                FULLY_WITHIN
            }
        }
        b'D' => {
            if val <= coord_max {
                NOT_WITHIN
            } else {
                FULLY_WITHIN
            }
        }
        b'E' => {
            if val >= coord_min {
                NOT_WITHIN
            } else {
                FULLY_WITHIN
            }
        }
        _ => FULLY_WITHIN,
    }
}

fn nonleaf_constraint(constraint: &RtreeConstraint, cell: &RtreeCell, n_dim2: usize) -> i32 {
    let coord_idx = constraint.i_coord;
    if coord_idx >= n_dim2 * 2 {
        return FULLY_WITHIN;
    }

    let coord_min = if coord_idx < n_dim2 {
        cell.coords[coord_idx]
    } else {
        cell.coords[n_dim2 + (coord_idx % n_dim2)]
    };
    let coord_max = if coord_idx < n_dim2 {
        cell.coords[n_dim2 + coord_idx]
    } else {
        cell.coords[coord_idx % n_dim2]
    };

    let val = constraint.value as f32;
    match constraint.op {
        b'A' => {
            if val < coord_min || val > coord_max {
                NOT_WITHIN
            } else {
                FULLY_WITHIN
            }
        }
        b'B' => {
            if val < coord_min {
                NOT_WITHIN
            } else {
                FULLY_WITHIN
            }
        }
        b'C' => {
            if val <= coord_max {
                NOT_WITHIN
            } else {
                FULLY_WITHIN
            }
        }
        b'D' => {
            if val <= coord_max {
                NOT_WITHIN
            } else {
                FULLY_WITHIN
            }
        }
        b'E' => {
            if val >= coord_min {
                NOT_WITHIN
            } else {
                FULLY_WITHIN
            }
        }
        _ => FULLY_WITHIN,
    }
}

/// Descend from `start` for `iterations` levels using the same heuristic as SQLite `ChooseLeaf` / [`choose_leaf`]
/// (minimum axis-aligned bounding area among children). Matches `for (ii < iDepth - iHeight)` when
/// `iterations = iDepth - iHeight` with `iDepth ≈ root.tree_depth() - 1` in our root encoding.
fn descend_from_root_with_start(
    table: &RtreeTable,
    conn: Option<&Arc<Connection>>,
    mut current: RtreeNode,
    _cell: &RtreeCell,
    n_dim2: usize,
    iterations: usize,
) -> Option<RtreeNode> {
    let mut depth = current.tree_depth();
    let mut remaining = iterations;

    while remaining > 0 {
        let n_cells = current.cell_count();
        if n_cells == 0 {
            return None;
        }
        let mut best_idx = 0;
        let mut best_area = f64::MAX;

        for i in 0..n_cells {
            let child_coords = current.coords(n_dim2, table.n_bytes_per_cell, i);
            let mut union_min = [f32::MAX; RTREE_MAX_DIMENSIONS];
            let mut union_max = [f32::MIN; RTREE_MAX_DIMENSIONS];

            for j in 0..n_dim2 {
                union_min[j] = union_min[j].min(child_coords[j]);
                union_min[j] = union_min[j].min(child_coords[n_dim2 + j]);
                union_max[j] = union_max[j].max(child_coords[j]);
                union_max[j] = union_max[j].max(child_coords[n_dim2 + j]);
            }

            let mut area = 1.0f64;
            for j in 0..n_dim2 {
                let size = (union_max[j] - union_min[j]) as f64;
                if size > 0.0 {
                    area *= size;
                }
            }

            if area < best_area {
                best_area = area;
                best_idx = i;
            }
        }

        let child_rowid = current
            .get_cell(n_dim2, table.n_bytes_per_cell, best_idx)
            .rowid;
        if let Some(c) = conn {
            if let Some(next) = table.read_node(c, child_rowid).ok().flatten() {
                current = next;
            } else {
                current = RtreeNode::new(child_rowid, depth - 1, table.node_size);
            }
        } else {
            current = RtreeNode::new(child_rowid, depth - 1, table.node_size);
        }
        depth -= 1;
        remaining -= 1;
    }

    Some(current)
}

fn choose_leaf(
    table: &RtreeTable,
    conn: Option<&Arc<Connection>>,
    cell: &RtreeCell,
    n_dim2: usize,
) -> Option<RtreeNode> {
    let root = match conn {
        Some(c) => table.load_root_node(c).ok()?,
        None => RtreeNode::new(1, 0, table.node_size),
    };
    let iterations = root.tree_depth().saturating_sub(1);
    descend_from_root_with_start(table, conn, root, cell, n_dim2, iterations)
}

/// Sum of `(max - min)` across every dimension (SQLite `cellMargin`).
fn cell_margin(cell: &RtreeCell, n_dim2: usize) -> f64 {
    let n_dim = n_dim2 / 2;
    let mut margin = 0.0f64;
    for j in 0..n_dim {
        margin += (cell.coords[2 * j + 1] - cell.coords[2 * j]) as f64;
    }
    margin
}

/// Product of `(max - min)` across every dimension (SQLite `cellArea`).
fn cell_area(cell: &RtreeCell, n_dim2: usize) -> f64 {
    let n_dim = n_dim2 / 2;
    let mut area = 1.0f64;
    for j in 0..n_dim {
        area *= (cell.coords[2 * j + 1] - cell.coords[2 * j]) as f64;
    }
    area
}

/// Volume of `a ∩ b`. Zero if disjoint in any dimension (SQLite `cellOverlap` with `nCell == 1`).
fn cell_pair_overlap_volume(a: &RtreeCell, b: &RtreeCell, n_dim2: usize) -> f64 {
    let n_dim = n_dim2 / 2;
    let mut o = 1.0f64;
    for j in 0..n_dim {
        let lo = a.coords[2 * j].max(b.coords[2 * j]) as f64;
        let hi = a.coords[2 * j + 1].min(b.coords[2 * j + 1]) as f64;
        if hi < lo {
            return 0.0;
        }
        o *= hi - lo;
    }
    o
}

/// R*-tree split (SQLite `splitNodeStartree`): pick the axis with minimum total margin, then within that axis the
/// split point with minimum overlap (tiebreak: area). The returned `bool` is `true` iff the freshly inserted `cell`
/// ended up in the right child — callers that need to follow the new cell (leaf insert, internal pointer reinsert)
/// use it to choose the next nodeno.
fn split_node(
    table: &RtreeTable,
    node: &RtreeNode,
    cell: &RtreeCell,
    n_dim2: usize,
) -> (RtreeNode, RtreeNode, bool) {
    let mut cells: Vec<RtreeCell> = (0..node.cell_count())
        .map(|i| node.get_cell(n_dim2, table.n_bytes_per_cell, i))
        .collect();
    let new_cell_orig_idx = cells.len();
    cells.push(cell.clone());
    let n_total = cells.len();
    let n_dim = n_dim2 / 2;
    let min_cells = table.min_cells().max(1);

    // SQLite's loop requires nLeft in [MINCELLS, nCell-MINCELLS]. For tiny inputs (only unit tests today) we fall
    // back to a halving split with the original (unsorted) cell order.
    if n_total < 2 * min_cells {
        let split_at = n_total.div_ceil(2).max(1).min(n_total);
        return build_split_children(
            table,
            node.depth,
            n_dim2,
            &cells,
            &(0..n_total).collect::<Vec<_>>(),
            split_at,
            new_cell_orig_idx,
        );
    }

    let mut best_split = min_cells;
    let mut best_margin_sum = f64::INFINITY;
    let mut best_order: Vec<usize> = (0..n_total).collect();
    let mut best_dim_seen = false;

    for d in 0..n_dim {
        let mut idx: Vec<usize> = (0..n_total).collect();
        // SQLite `SortByDimension`: primary key is min coord on dim d, tiebreak by max coord on dim d.
        idx.sort_by(|&a, &b| {
            let a_lo = cells[a].coords[2 * d];
            let b_lo = cells[b].coords[2 * d];
            match a_lo.partial_cmp(&b_lo) {
                Some(std::cmp::Ordering::Equal) | None => {
                    let a_hi = cells[a].coords[2 * d + 1];
                    let b_hi = cells[b].coords[2 * d + 1];
                    a_hi.partial_cmp(&b_hi).unwrap_or(std::cmp::Ordering::Equal)
                }
                Some(ord) => ord,
            }
        });

        let mut margin_sum = 0.0f64;
        let mut best_overlap_for_dim = f64::INFINITY;
        let mut best_area_for_dim = f64::INFINITY;
        let mut best_split_for_dim = min_cells;

        for n_left in min_cells..=(n_total - min_cells) {
            let mut left = cells[idx[0]].clone();
            for k in 1..n_left {
                left = cell_union_mbr_coords(&left, &cells[idx[k]], n_dim2);
            }
            let mut right = cells[idx[n_left]].clone();
            for k in (n_left + 1)..n_total {
                right = cell_union_mbr_coords(&right, &cells[idx[k]], n_dim2);
            }

            margin_sum += cell_margin(&left, n_dim2) + cell_margin(&right, n_dim2);
            let overlap = cell_pair_overlap_volume(&left, &right, n_dim2);
            let area = cell_area(&left, n_dim2) + cell_area(&right, n_dim2);

            let take = n_left == min_cells
                || overlap < best_overlap_for_dim
                || (overlap == best_overlap_for_dim && area < best_area_for_dim);
            if take {
                best_split_for_dim = n_left;
                best_overlap_for_dim = overlap;
                best_area_for_dim = area;
            }
        }

        if !best_dim_seen || margin_sum < best_margin_sum {
            best_dim_seen = true;
            best_margin_sum = margin_sum;
            best_split = best_split_for_dim;
            best_order = idx;
        }
    }

    build_split_children(
        table,
        node.depth,
        n_dim2,
        &cells,
        &best_order,
        best_split,
        new_cell_orig_idx,
    )
}

fn build_split_children(
    table: &RtreeTable,
    node_depth: usize,
    n_dim2: usize,
    cells: &[RtreeCell],
    order: &[usize],
    split_at: usize,
    new_cell_orig_idx: usize,
) -> (RtreeNode, RtreeNode, bool) {
    let mut left_node = RtreeNode::new(0, node_depth, table.node_size);
    let mut right_node = RtreeNode::new(0, node_depth, table.node_size);
    let mut left_count = 0;
    let mut right_count = 0;
    let mut new_in_right = false;

    for (sorted_pos, &ci) in order.iter().enumerate() {
        if sorted_pos < split_at {
            left_node.set_cell(n_dim2, table.n_bytes_per_cell, left_count, &cells[ci]);
            left_count += 1;
        } else {
            right_node.set_cell(n_dim2, table.n_bytes_per_cell, right_count, &cells[ci]);
            right_count += 1;
            if ci == new_cell_orig_idx {
                new_in_right = true;
            }
        }
    }

    left_node.set_cell_count(left_count);
    right_node.set_cell_count(right_count);
    (left_node, right_node, new_in_right)
}

/// Bounding box for all entries in `node`, stored as an internal-node cell referencing child `child_nodeno`.
fn bounding_cell_for_child_node(
    table: &RtreeTable,
    node: &RtreeNode,
    child_nodeno: i64,
    n_dim2: usize,
) -> RtreeCell {
    let n_dim = n_dim2 / 2;
    let mut out = RtreeCell::new(child_nodeno);
    let n_cells = node.cell_count();
    if n_cells == 0 {
        return out;
    }
    for j in 0..n_dim {
        let mut mn = f32::MAX;
        let mut mx = f32::MIN;
        for i in 0..n_cells {
            let cell = node.get_cell(n_dim2, table.n_bytes_per_cell, i);
            mn = mn.min(cell.coords[2 * j]);
            mx = mx.max(cell.coords[2 * j + 1]);
        }
        out.coords[2 * j] = mn;
        out.coords[2 * j + 1] = mx;
    }
    out
}

/// Axis-aligned union of two bounding boxes (`coords[2*d]` min, `coords[2*d+1]` max per dimension).
fn cell_union_mbr_coords(a: &RtreeCell, b: &RtreeCell, n_dim2: usize) -> RtreeCell {
    let n_dim = n_dim2 / 2;
    let mut out = RtreeCell::new(a.rowid);
    for j in 0..n_dim {
        out.coords[2 * j] = a.coords[2 * j].min(b.coords[2 * j]);
        out.coords[2 * j + 1] = a.coords[2 * j + 1].max(b.coords[2 * j + 1]);
    }
    out
}

/// True if `outer` fully contains `inner` in every dimension (SQLite `cellContains`).
fn cell_contains_mbr(outer: &RtreeCell, inner: &RtreeCell, n_dim2: usize) -> bool {
    let n_dim = n_dim2 / 2;
    for j in 0..n_dim {
        let o_lo = outer.coords[2 * j];
        let o_hi = outer.coords[2 * j + 1];
        let i_lo = inner.coords[2 * j];
        let i_hi = inner.coords[2 * j + 1];
        if i_lo < o_lo || i_hi > o_hi {
            return false;
        }
    }
    true
}

fn union_mbr_of_node_cells(
    table: &RtreeTable,
    node: &RtreeNode,
    n_dim2: usize,
) -> Option<RtreeCell> {
    let n = node.cell_count();
    if n == 0 {
        return None;
    }
    let mut acc = node.get_cell(n_dim2, table.n_bytes_per_cell, 0);
    for i in 1..n {
        let c = node.get_cell(n_dim2, table.n_bytes_per_cell, i);
        acc = cell_union_mbr_coords(&acc, &c, n_dim2);
    }
    Some(acc)
}

impl VTable for RtreeTable {
    type Cursor = RtreeCursor;
    type Error = ResultCode;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        Ok(RtreeCursor::new(
            conn,
            self.n_dim2,
            self.n_bytes_per_cell,
            self.node_size,
            self.table_name.clone(),
            self.aux_columns.clone(),
        ))
    }

    fn begin(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn update(
        &mut self,
        conn: Option<Arc<Connection>>,
        old_rowid: i64,
        args: &[Value],
    ) -> Result<(), Self::Error> {
        let Some(conn) = conn else {
            return Err(ResultCode::InvalidArgs);
        };
        let merged = self.merge_xupdate_argv_with_existing_row(&conn, old_rowid, args)?;
        self.delete(Some(conn.clone()), old_rowid)?;
        self.insert(Some(conn), &merged)?;
        Ok(())
    }

    fn insert(
        &mut self,
        conn: Option<Arc<Connection>>,
        args: &[Value],
    ) -> Result<i64, Self::Error> {
        let Some(conn) = conn else {
            return Err(ResultCode::InvalidArgs);
        };
        // xUpdate passes argv[2..] as `columns`: id + coordinate pairs + optional aux (see vtab_derive).
        let required = self.n_dim2 + 1 + self.aux_columns.len();
        if args.len() < required {
            return Err(ResultCode::InvalidArgs);
        }
        let rowid = match args.first().and_then(|v| v.to_integer()) {
            Some(id) => {
                self.row_count = self.row_count.max(id);
                id
            }
            None => {
                self.row_count += 1;
                self.row_count
            }
        };

        let mut cell = RtreeCell::new(rowid);
        for i in 0..self.n_dim2 {
            if let Some(val) = args[i + 1].to_float() {
                cell.coords[i] = val as f32;
            }
        }

        let aux = &args[self.n_dim2 + 1..required];
        self.insert_leaf_row(&conn, rowid, &cell, aux)?;

        Ok(rowid)
    }

    fn delete(&mut self, conn: Option<Arc<Connection>>, rowid: i64) -> Result<(), Self::Error> {
        let Some(conn) = conn else {
            return Err(ResultCode::InvalidArgs);
        };
        let mut pending = Vec::new();
        if let Some(nodeno) = self.get_rowid_nodeno(&conn, rowid)? {
            if let Some(mut node) = self.read_node(&conn, nodeno)? {
                if let Some(cell_idx) = self.find_cell_index(&node, rowid) {
                    self.delete_cell_from_node(&mut node, cell_idx);
                    self.write_node(&conn, nodeno, &node)?;

                    let mut depth_guard = 0;
                    self.fix_after_cell_removal(&conn, nodeno, 0, &mut pending, &mut depth_guard)?;
                }
            }
        }
        // Mirror `rtreeDeleteRowid`: rowid shadow delete, then optional root collapse, then reinsert.
        self.delete_rowid_map(&conn, rowid)?;
        self.maybe_collapse_root_single_child(&conn, &mut pending)?;

        for (cells, h) in pending {
            for c in cells {
                if h == 0 {
                    let aux = match self.read_rowid_aux_values(&conn, c.rowid) {
                        Ok(a) => a,
                        Err(ResultCode::NotFound) => (0..self.aux_columns.len())
                            .map(|_| Value::null())
                            .collect(),
                        Err(e) => return Err(e),
                    };
                    self.insert_leaf_row(&conn, c.rowid, &c, &aux)?;
                } else {
                    self.insert_internal_cell_at_height(&conn, &c, h)?;
                }
            }
        }
        Ok(())
    }

    fn destroy(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let mut idx_num = -1;
        let mut idx_str = None;
        let mut estimated_cost = 1000000.0;
        let mut estimated_rows = (RTREE_DEFAULT_ROWEST as u32).max(RTREE_MIN_ROWEST);
        let mut constraint_usages = Vec::with_capacity(constraints.len());

        for constraint in constraints {
            if !constraint.usable {
                constraint_usages.push(ConstraintUsage {
                    argv_index: None,
                    omit: false,
                });
                continue;
            }

            if constraint.column_index == 0 && constraint.op == ConstraintOp::Eq {
                constraint_usages.push(ConstraintUsage {
                    argv_index: Some(1),
                    omit: true,
                });
                idx_num = IDX_NUM_ROWID;
                idx_str = Some(IDX_STR_ROWID.to_string());
                estimated_cost = 30.0;
                estimated_rows = 1;
            } else if constraint.column_index >= 1 {
                if let Some(rtree_op) = constraint_op_to_rtree_op(constraint.op) {
                    let idx = constraint_usages.len() + 1;
                    constraint_usages.push(ConstraintUsage {
                        argv_index: Some(idx as u32),
                        omit: true,
                    });
                    if idx_num == -1 {
                        idx_num = IDX_NUM_QUERY;
                    }
                    let op_char = rtree_op as char;
                    let coord_char = ((constraint.column_index - 1) % 10) as u8 as char;
                    let pair = format!("{}{}", op_char, coord_char);
                    if let Some(ref mut s) = idx_str {
                        s.push_str(&pair);
                    } else {
                        idx_str = Some(pair);
                    }
                } else {
                    constraint_usages.push(ConstraintUsage {
                        argv_index: None,
                        omit: false,
                    });
                }
            } else {
                constraint_usages.push(ConstraintUsage {
                    argv_index: None,
                    omit: false,
                });
            }
        }

        if idx_num == -1 {
            idx_num = IDX_NUM_QUERY;
            idx_str = Some(IDX_STR_QUERY.to_string());
        }

        Ok(IndexInfo {
            idx_num,
            idx_str,
            order_by_consumed: false,
            estimated_cost,
            estimated_rows,
            constraint_usages,
        })
    }
}

impl VTabCursor for RtreeCursor {
    type Error = ResultCode;

    fn filter(&mut self, args: &[Value], idx_info: Option<(&str, i32)>) -> ResultCode {
        self.at_eof = true;
        self.constraints.clear();
        self.aux_values.clear();
        self.rowid = 0;

        let idx_str = idx_info.map(|(s, _)| s).unwrap_or("query");

        if idx_str == "rowid_lookup" && !args.is_empty() {
            if let Some(id_val) = args.first() {
                if let Some(id) = id_val.to_integer() {
                    if let Some(conn) = &self.conn {
                        let shadow_rowid_table = format!("{}_rowid", self.table_name);
                        let sql =
                            format!("SELECT nodeno FROM {} WHERE rowid = ?", shadow_rowid_table);
                        if let Ok(mut stmt) = conn.prepare(&sql) {
                            stmt.bind_at(
                                std::num::NonZeroUsize::new(1).unwrap(),
                                Value::from_integer(id),
                            );
                            if stmt.step() == StepResult::Row {
                                let row = stmt.get_row();
                                if let Some(val) = row.first() {
                                    if let Some(nodeno) = val.to_integer() {
                                        if let Some(node) = self.load_node(nodeno) {
                                            for i in 0..node.cell_count() {
                                                let cell = node.get_cell(
                                                    self.n_dim2,
                                                    self.n_bytes_per_cell,
                                                    i,
                                                );
                                                if cell.rowid == id {
                                                    self.rowid = id;
                                                    self.current_coords[..self.n_dim2 * 2]
                                                        .copy_from_slice(
                                                            &cell.coords[..self.n_dim2 * 2],
                                                        );
                                                    self.at_eof = false;
                                                    if self.load_aux_values() != Ok(()) {
                                                        return ResultCode::Error;
                                                    }
                                                    return ResultCode::OK;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            return ResultCode::OK;
        }

        let idx_bytes = idx_str.as_bytes();
        let mut arg_idx = 0;

        for pair_idx in (0..idx_bytes.len()).step_by(2) {
            if pair_idx + 1 >= idx_bytes.len() {
                break;
            }
            let op = idx_bytes[pair_idx];
            let coord_digit = idx_bytes[pair_idx + 1];
            let coord_idx = ((coord_digit as usize) - (b'0' as usize)) * 2;

            if arg_idx < args.len() {
                if let Some(f) = args[arg_idx].to_float() {
                    self.constraints.push(RtreeConstraint {
                        i_coord: coord_idx,
                        op,
                        value: f,
                    });
                }
            }
            arg_idx += 1;
        }

        if self.constraints.is_empty() {
            self.at_eof = false;
            return ResultCode::OK;
        }

        if let Some(_conn) = &self.conn {
            if let Some(root) = self.load_node(1) {
                let depth = root.tree_depth();
                let mut current_node = root;
                let current_level = if depth == 0 { 0 } else { depth as i32 };
                let mut current_cell_idx = 0usize;
                let mut level = current_level;

                loop {
                    if level <= 0 {
                        let n_cells = current_node.cell_count();
                        while current_cell_idx < n_cells {
                            let cell = current_node.get_cell(
                                self.n_dim2,
                                self.n_bytes_per_cell,
                                current_cell_idx,
                            );

                            let mut matches = true;
                            for constraint in &self.constraints {
                                let result = leaf_constraint(constraint, &cell, self.n_dim2);
                                if result == NOT_WITHIN {
                                    matches = false;
                                    break;
                                }
                            }

                            if matches {
                                self.rowid = cell.rowid;
                                self.current_coords[..self.n_dim2 * 2]
                                    .copy_from_slice(&cell.coords[..self.n_dim2 * 2]);
                                self.at_eof = false;
                                if self.load_aux_values() != Ok(()) {
                                    return ResultCode::Error;
                                }
                                return ResultCode::OK;
                            }

                            current_cell_idx += 1;
                        }
                        break;
                    } else {
                        let n_cells = current_node.cell_count();
                        let mut found_child = false;

                        while current_cell_idx < n_cells {
                            let cell = current_node.get_cell(
                                self.n_dim2,
                                self.n_bytes_per_cell,
                                current_cell_idx,
                            );

                            let mut matches = true;
                            for constraint in &self.constraints {
                                let result = nonleaf_constraint(constraint, &cell, self.n_dim2);
                                if result == NOT_WITHIN {
                                    matches = false;
                                    break;
                                }
                            }

                            if matches {
                                if let Some(child_node) = self.load_node(cell.rowid) {
                                    current_node = child_node;
                                    level -= 1;
                                    current_cell_idx = 0;
                                    found_child = true;
                                    break;
                                }
                            }

                            current_cell_idx += 1;
                        }

                        if !found_child {
                            break;
                        }
                    }
                }
            }
        }

        self.at_eof = true;
        ResultCode::OK
    }

    fn rowid(&self) -> i64 {
        self.rowid
    }

    fn column(&self, idx: u32) -> Result<Value, Self::Error> {
        if idx == 0 {
            return Ok(Value::from_integer(self.rowid));
        }
        let i = idx as usize;
        if i <= self.n_dim2 {
            return Ok(Value::from_float(self.current_coords[i - 1] as f64));
        }
        let aux_i = i - 1 - self.n_dim2;
        if aux_i < self.aux_values.len() {
            return Ok(value_to_owned(&self.aux_values[aux_i]));
        }
        Ok(Value::null())
    }

    fn eof(&self) -> bool {
        self.at_eof
    }

    fn next(&mut self) -> ResultCode {
        if self.constraints.is_empty() || self.rowid == 0 {
            self.at_eof = true;
            return ResultCode::EOF;
        }

        let saved_rowid = self.rowid;

        if let Some(_conn) = &self.conn {
            if let Some(root) = self.load_node(1) {
                let depth = root.tree_depth();
                let mut current_node = root;
                let current_level = if depth == 0 { 0 } else { depth as i32 };
                let mut current_cell_idx = 0usize;
                let mut level = current_level;

                loop {
                    if level <= 0 {
                        let n_cells = current_node.cell_count();

                        while current_cell_idx < n_cells {
                            let cell = current_node.get_cell(
                                self.n_dim2,
                                self.n_bytes_per_cell,
                                current_cell_idx,
                            );

                            if cell.rowid <= saved_rowid {
                                current_cell_idx += 1;
                                continue;
                            }

                            let mut matches = true;
                            for constraint in &self.constraints {
                                let result = leaf_constraint(constraint, &cell, self.n_dim2);
                                if result == NOT_WITHIN {
                                    matches = false;
                                    break;
                                }
                            }

                            if matches {
                                self.rowid = cell.rowid;
                                self.current_coords[..self.n_dim2 * 2]
                                    .copy_from_slice(&cell.coords[..self.n_dim2 * 2]);
                                self.at_eof = false;
                                if self.load_aux_values() != Ok(()) {
                                    return ResultCode::Error;
                                }
                                return ResultCode::OK;
                            }

                            current_cell_idx += 1;
                        }
                        break;
                    } else {
                        let n_cells = current_node.cell_count();
                        let mut found_child = false;

                        while current_cell_idx < n_cells {
                            let cell = current_node.get_cell(
                                self.n_dim2,
                                self.n_bytes_per_cell,
                                current_cell_idx,
                            );

                            let mut matches = true;
                            for constraint in &self.constraints {
                                let result = nonleaf_constraint(constraint, &cell, self.n_dim2);
                                if result == NOT_WITHIN {
                                    matches = false;
                                    break;
                                }
                            }

                            if matches {
                                if let Some(child_node) = self.load_node(cell.rowid) {
                                    current_node = child_node;
                                    level -= 1;
                                    current_cell_idx = 0;
                                    found_child = true;
                                    break;
                                }
                            }

                            current_cell_idx += 1;
                        }

                        if !found_child {
                            break;
                        }
                    }
                }
            }
        }

        self.at_eof = true;
        ResultCode::EOF
    }

    fn close(&self) -> ResultCode {
        ResultCode::OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_table(args: Vec<&str>) -> (String, RtreeTable) {
        let args: Vec<Value> = args
            .iter()
            .map(|s| Value::from_text(s.to_string()))
            .collect();
        RtreeModule::create(&args).unwrap()
    }

    fn new_table(args: Vec<&str>) -> RtreeTable {
        create_table(args).1
    }

    #[test]
    fn test_create_table() {
        let table = new_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax",
        ]);
        assert_eq!(table.n_dim2, 4);
        assert_eq!(table.n_dim2 / 2, 2);
        assert!(table.aux_columns.is_empty());
    }

    #[test]
    fn test_create_table_with_aux_column() {
        let table = new_table(vec![
            "rtree", "main", "t", "id", "xmin", "xmax", "ymin", "ymax", "+label",
        ]);
        assert_eq!(table.n_dim2, 4);
        assert_eq!(table.aux_columns, vec!["label".to_string()]);
    }

    #[test]
    fn test_parse_aux_column_with_type_token() {
        let table = new_table(vec![
            "rtree",
            "main",
            "t",
            "id",
            "xmin",
            "xmax",
            "ymin",
            "ymax",
            "+label TEXT",
        ]);
        assert_eq!(table.aux_columns, vec!["label".to_string()]);
    }

    #[test]
    fn test_parse_multiple_aux_columns() {
        let table = new_table(vec![
            "rtree",
            "main",
            "t",
            "id",
            "xmin",
            "xmax",
            "ymin",
            "ymax",
            "+a",
            "+b INTEGER",
        ]);
        assert_eq!(table.aux_columns, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn test_virtual_table_declaration_quotes_aux_identifiers() {
        let (schema, _) = create_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax", "+select",
        ]);
        assert!(
            schema.contains("\"select\" TEXT"),
            "aux schema must quote SQL keywords: {schema}"
        );
    }

    #[test]
    fn test_node_creation() {
        let node = RtreeNode::new(1, 0, 4096 - 64);
        assert_eq!(node.node_no, 1);
        assert_eq!(node.cell_count(), 0);
    }

    #[test]
    fn test_max_cells() {
        let table = new_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax",
        ]);
        let max_cells = table.max_cells();
        assert!(max_cells > 0);
        assert_eq!(table.min_cells(), max_cells / 3);
    }

    #[test]
    fn test_cursor_creation() {
        let table = new_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax",
        ]);
        let cursor = VTable::open(&table, None).unwrap();
        assert!(cursor.eof());
    }

    #[test]
    fn test_node_read_write_coord() {
        let mut node = RtreeNode::new(1, 0, 4096 - 64);
        node.write_coord(100, 3.14159);
        let val = node.read_coord(100);
        assert!((val - 3.14159).abs() < 0.0001);
    }

    #[test]
    fn test_node_read_write_int64() {
        let mut node = RtreeNode::new(1, 0, 4096 - 64);
        node.write_int64(50, 1234567890i64);
        let val = node.read_int64(50);
        assert_eq!(val, 1234567890i64);
    }

    #[test]
    fn test_node_cell_operations() {
        let n_dim2 = 4;
        let n_bytes_per_cell = 8 + n_dim2 * 4;
        let mut node = RtreeNode::new(1, 0, 4096 - 64);

        let mut cell = RtreeCell::new(42);
        cell.coords[0] = 1.0;
        cell.coords[1] = 5.0;
        cell.coords[2] = 2.0;
        cell.coords[3] = 6.0;

        node.set_cell(n_dim2, n_bytes_per_cell, 0, &cell);
        node.set_cell_count(1);

        assert_eq!(node.cell_count(), 1);
        let read_cell = node.get_cell(n_dim2, n_bytes_per_cell, 0);
        assert_eq!(read_cell.rowid, 42);
        assert!((read_cell.coords[0] - 1.0).abs() < 0.0001);
        assert!((read_cell.coords[1] - 5.0).abs() < 0.0001);
        assert!((read_cell.coords[2] - 2.0).abs() < 0.0001);
        assert!((read_cell.coords[3] - 6.0).abs() < 0.0001);
    }

    #[test]
    fn test_node_depth() {
        let mut node = RtreeNode::new(1, 5, 4096 - 64);
        assert_eq!(node.tree_depth(), 5);
        node.set_depth(3);
        assert_eq!(node.tree_depth(), 3);
    }

    #[test]
    fn test_leaf_constraint() {
        let mut coords = [0.0; RTREE_MAX_DIMENSIONS * 2];
        coords[0] = 5.0; // xmin
        coords[1] = 10.0; // xmax
        coords[2] = 3.0; // ymin
        coords[3] = 7.0; // ymax
        let mut test_cell = RtreeCell::new(1);
        test_cell.coords = coords;

        let constraint = RtreeConstraint {
            i_coord: 0,
            op: b'E',
            value: 15.0,
        };

        let result = leaf_constraint(&constraint, &test_cell, 4);
        assert_eq!(result, NOT_WITHIN);
    }

    #[test]
    fn test_nonleaf_constraint() {
        let mut coords = [0.0; RTREE_MAX_DIMENSIONS * 2];
        coords[0] = 5.0;
        coords[1] = 10.0;
        let mut test_cell = RtreeCell::new(1);
        test_cell.coords = coords;

        let constraint = RtreeConstraint {
            i_coord: 0,
            op: b'B',
            value: 4.0,
        };

        let result = nonleaf_constraint(&constraint, &test_cell, 4);
        assert_eq!(result, NOT_WITHIN);
    }

    #[test]
    fn test_choose_leaf() {
        let table = new_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax",
        ]);
        let cell = RtreeCell::new(1);
        let leaf = choose_leaf(&table, None, &cell, 4);
        assert!(leaf.is_some());
    }

    /// With `tree_depth == 1` on the root blob, `ChooseLeaf` runs `tree_depth - 1 == 0` descent steps (SQLite `iDepth`
    /// mapping): the chosen node stays the root.
    #[test]
    fn test_choose_leaf_zero_descent_when_single_tree_level() {
        let table = new_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax",
        ]);
        let cell = RtreeCell::new(42);
        let root = RtreeNode::new(1, 1, table.node_size);
        let leaf =
            descend_from_root_with_start(&table, None, root, &cell, table.n_dim2, 0).unwrap();
        assert_eq!(leaf.node_no, 1);
        assert_eq!(leaf.tree_depth(), 1);
    }

    #[test]
    fn test_split_node() {
        let table = new_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax",
        ]);
        let mut node = RtreeNode::new(1, 0, 4096 - 64);
        node.set_cell_count(0);

        let cell = RtreeCell::new(1);
        let (left, right, _) = split_node(&table, &node, &cell, 4);

        assert!(left.cell_count() > 0 || right.cell_count() > 0);
    }

    /// Two well-separated clusters on the X axis — the R*-tree heuristic must group them on that axis with zero
    /// overlap, regardless of the order cells were appended. Verifies parity with SQLite `splitNodeStartree`.
    #[test]
    fn test_split_node_groups_clusters_zero_overlap() {
        let table = new_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax",
        ]);
        let n_dim2 = 4;
        let n_bytes = table.n_bytes_per_cell;
        let mut node = RtreeNode::new(1, 0, table.node_size);

        // Force the per-axis loop to run (need N >= 2 * min_cells). Build alternating cluster cells.
        let m = (table.min_cells() * 2 + 2).max(8);
        let left_cluster_count = m / 2;
        let mut count = 0;
        for i in 0..left_cluster_count {
            // Left cluster: x in [0, 1], y wandering over [0, 100].
            let mut c = RtreeCell::new((i as i64) + 1);
            c.coords[0] = 0.0;
            c.coords[1] = 1.0;
            c.coords[2] = i as f32;
            c.coords[3] = (i as f32) + 0.5;
            node.set_cell(n_dim2, n_bytes, count, &c);
            count += 1;
        }
        let right_cluster_count = m - 1 - left_cluster_count;
        for i in 0..right_cluster_count {
            // Right cluster: x in [100, 101], y wandering.
            let mut c = RtreeCell::new((left_cluster_count + i + 1) as i64);
            c.coords[0] = 100.0;
            c.coords[1] = 101.0;
            c.coords[2] = i as f32;
            c.coords[3] = (i as f32) + 0.5;
            node.set_cell(n_dim2, n_bytes, count, &c);
            count += 1;
        }
        node.set_cell_count(count);

        // The new cell joins the right cluster.
        let mut new_cell = RtreeCell::new(9999);
        new_cell.coords[0] = 100.5;
        new_cell.coords[1] = 101.0;
        new_cell.coords[2] = 0.0;
        new_cell.coords[3] = 0.5;

        let (left, right, new_in_right) = split_node(&table, &node, &new_cell, n_dim2);
        assert!(left.cell_count() >= table.min_cells());
        assert!(right.cell_count() >= table.min_cells());

        let left_bbox = union_mbr_of_node_cells(&table, &left, n_dim2).unwrap();
        let right_bbox = union_mbr_of_node_cells(&table, &right, n_dim2).unwrap();
        // Disjoint MBRs on the X axis prove the split picked the right partition.
        assert!(
            left_bbox.coords[1] < right_bbox.coords[0]
                || right_bbox.coords[1] < left_bbox.coords[0],
            "expected disjoint X projections after R*-tree split: left=[{},{}], right=[{},{}]",
            left_bbox.coords[0],
            left_bbox.coords[1],
            right_bbox.coords[0],
            right_bbox.coords[1],
        );
        assert!(
            new_in_right,
            "new cell from the right cluster should land in right child"
        );
    }

    #[test]
    fn test_cell_pair_overlap_volume_disjoint_and_nested() {
        let mut a = RtreeCell::new(1);
        a.coords[0] = 0.0;
        a.coords[1] = 1.0;
        a.coords[2] = 0.0;
        a.coords[3] = 1.0;

        let mut b = RtreeCell::new(2);
        b.coords[0] = 2.0;
        b.coords[1] = 3.0;
        b.coords[2] = 0.0;
        b.coords[3] = 1.0;
        // Disjoint on x.
        assert_eq!(cell_pair_overlap_volume(&a, &b, 4), 0.0);

        let mut c = RtreeCell::new(3);
        c.coords[0] = 0.25;
        c.coords[1] = 0.75;
        c.coords[2] = 0.25;
        c.coords[3] = 0.75;
        // Nested inside a: overlap = 0.5 * 0.5 = 0.25.
        let o = cell_pair_overlap_volume(&a, &c, 4);
        assert!((o - 0.25).abs() < 1e-6, "got {o}");

        assert!((cell_margin(&a, 4) - 2.0).abs() < 1e-6);
        assert!((cell_area(&a, 4) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cell_union_and_contains_mbr() {
        let mut a = RtreeCell::new(1);
        a.coords[0] = 0.0;
        a.coords[1] = 2.0;
        a.coords[2] = 1.0;
        a.coords[3] = 3.0;

        let mut b = RtreeCell::new(2);
        b.coords[0] = 1.0;
        b.coords[1] = 5.0;
        b.coords[2] = 0.0;
        b.coords[3] = 2.0;

        let u = cell_union_mbr_coords(&a, &b, 4);
        assert!((u.coords[0] - 0.0).abs() < 1e-5);
        assert!((u.coords[1] - 5.0).abs() < 1e-5);
        assert!((u.coords[2] - 0.0).abs() < 1e-5);
        assert!((u.coords[3] - 3.0).abs() < 1e-5);

        assert!(cell_contains_mbr(&u, &a, 4));
        assert!(cell_contains_mbr(&u, &b, 4));
    }

    #[test]
    fn test_bounding_cell_for_child_node() {
        let table = new_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax",
        ]);
        let n_dim2 = 4;
        let n_bytes = table.n_bytes_per_cell;
        let mut node = RtreeNode::new(1, 0, 4096 - 64);

        let mut a = RtreeCell::new(1);
        a.coords[0] = 0.0;
        a.coords[1] = 1.0;
        a.coords[2] = 0.0;
        a.coords[3] = 1.0;

        let mut b = RtreeCell::new(2);
        b.coords[0] = 2.0;
        b.coords[1] = 10.0;
        b.coords[2] = 3.0;
        b.coords[3] = 4.0;

        node.set_cell(n_dim2, n_bytes, 0, &a);
        node.set_cell(n_dim2, n_bytes, 1, &b);
        node.set_cell_count(2);

        let bc = bounding_cell_for_child_node(&table, &node, 99, n_dim2);
        assert_eq!(bc.rowid, 99);
        assert!((bc.coords[0] - 0.0).abs() < 1e-5);
        assert!((bc.coords[1] - 10.0).abs() < 1e-5);
        assert!((bc.coords[2] - 0.0).abs() < 1e-5);
        assert!((bc.coords[3] - 4.0).abs() < 1e-5);
    }

    #[test]
    fn test_delete_cell_from_node() {
        let mut node = RtreeNode::new(1, 0, 4096 - 64);
        let n_dim2 = 4;
        let n_bytes_per_cell = 8 + n_dim2 * 4;

        let mut cell1 = RtreeCell::new(1);
        cell1.coords[0] = 1.0;
        cell1.coords[1] = 5.0;
        cell1.coords[2] = 2.0;
        cell1.coords[3] = 6.0;

        let mut cell2 = RtreeCell::new(2);
        cell2.coords[0] = 3.0;
        cell2.coords[1] = 7.0;
        cell2.coords[2] = 4.0;
        cell2.coords[3] = 8.0;

        node.set_cell(n_dim2, n_bytes_per_cell, 0, &cell1);
        node.set_cell(n_dim2, n_bytes_per_cell, 1, &cell2);
        node.set_cell_count(2);

        assert_eq!(node.cell_count(), 2);

        let table = new_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax",
        ]);

        table.delete_cell_from_node(&mut node, 0);

        assert_eq!(node.cell_count(), 1);

        let remaining_cell = node.get_cell(n_dim2, n_bytes_per_cell, 0);
        assert_eq!(remaining_cell.rowid, 2);
    }

    #[test]
    fn test_update_requires_conn() {
        let mut table = new_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax",
        ]);
        let args = [
            Value::from_integer(1),
            Value::from_float(0.0),
            Value::from_float(1.0),
            Value::from_float(0.0),
            Value::from_float(1.0),
        ];
        assert_eq!(
            VTable::update(&mut table, None, 1, &args),
            Err(ResultCode::InvalidArgs)
        );
    }
}
