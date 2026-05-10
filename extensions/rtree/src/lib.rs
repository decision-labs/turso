//! R-Tree virtual table extension.
//!
//! A Rust implementation of SQLite's rtree extension for spatial indexing.
//! Uses the R-Tree algorithm from Guttman[1984].
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

use std::sync::Arc;
use turso_ext::{
    register_extension, Connection, ConstraintInfo, ConstraintOp, ConstraintUsage, IndexInfo,
    OrderByInfo, ResultCode, StepResult, VTabCursor, VTabKind, VTabModule, VTabModuleDerive,
    VTable, Value,
};

register_extension! {
    vtabs: { RtreeModule }
}

const RTREE_MAX_DIMENSIONS: usize = 5;
const RTREE_MAX_DEPTH: usize = 40;
const RTREE_CACHE_SZ: usize = 5;
const RTREE_DEFAULT_ROWEST: i64 = 1048576;
const RTREE_MAXCELLS: usize = 51;

const RTREE_COORD_REAL32: u8 = 0;
const RTREE_COORD_INT32: u8 = 1;

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
    fn parse_column_args(args: &[Value]) -> Result<(usize, usize), ResultCode> {
        let mut n_dim2 = 0;
        let mut n_aux = 0;
        for arg in args {
            if let Some(text) = arg.to_text() {
                if text.starts_with('+') {
                    n_aux += 1;
                } else if n_aux == 0 {
                    n_dim2 += 1;
                } else {
                    break;
                }
            } else {
                return Err(ResultCode::Error);
            }
        }
        if n_dim2 < 2 || n_dim2 > RTREE_MAX_DIMENSIONS * 2 || n_dim2 % 2 != 0 {
            return Err(ResultCode::Error);
        }
        Ok((n_dim2, n_aux))
    }
}

impl VTabModule for RtreeModule {
    type Table = RtreeTable;
    const VTAB_KIND: VTabKind = VTabKind::VirtualTable;
    const NAME: &'static str = "rtree";
    const READONLY: bool = false;

    fn create(args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        if args.len() < 5 {
            return Err(ResultCode::Error);
        }

        let (n_dim2, n_aux) = Self::parse_column_args(&args[4..])?;
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
        let schema = format!("CREATE TABLE x ({})", columns);

        let table_name = args
            .get(2)
            .and_then(|v| v.to_text())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "x".to_string());

        let table = RtreeTable {
            n_dim,
            n_dim2,
            n_bytes_per_cell,
            n_aux,
            e_coord_type: RTREE_COORD_REAL32,
            node_size: default_node_size,
            depth: 0,
            row_count: 0,
            aux_columns: vec![],
            node_count: 0,
            table_name,
        };

        Ok((schema, table))
    }
}

#[derive(Debug, Clone)]
struct RtreeTable {
    n_dim: usize,
    n_dim2: usize,
    n_bytes_per_cell: usize,
    n_aux: usize,
    e_coord_type: u8,
    node_size: usize,
    depth: usize,
    row_count: i64,
    aux_columns: Vec<String>,
    node_count: i64,
    table_name: String,
}

impl RtreeTable {
    fn max_cells(&self) -> usize {
        (self.node_size - 4) / self.n_bytes_per_cell
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
                        parent: None,
                        n_ref: 1,
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

        let rowid_sql = format!(
            "CREATE TABLE {} (rowid INTEGER PRIMARY KEY, nodeno INTEGER)",
            self.shadow_rowid_table()
        );
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
                        parent: None,
                        n_ref: 1,
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
    ) -> Result<(), ResultCode> {
        let sql = format!(
            "INSERT OR REPLACE INTO {} (rowid, nodeno) VALUES (?, ?)",
            self.shadow_rowid_table()
        );
        conn.execute(
            &sql,
            &[Value::from_integer(rowid), Value::from_integer(nodeno)],
        )
        .map_err(|_| ResultCode::Error)?;
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
    parent: Option<Arc<RtreeNode>>,
    n_ref: usize,
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
            parent: None,
            n_ref: 1,
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

    fn bounding_box(&self, n_dim2: usize, n_bytes_per_cell: usize) -> RtreeCell {
        let n_cells = self.cell_count();
        let mut bbox = RtreeCell::new(0);
        for i in 0..n_dim2 {
            bbox.coords[i] = f32::MAX;
            bbox.coords[n_dim2 + i] = f32::MIN;
        }
        for i in 0..n_cells {
            let cell_coords = self.coords(n_dim2, n_bytes_per_cell, i);
            for j in 0..n_dim2 {
                bbox.coords[j] = bbox.coords[j].min(cell_coords[j]);
                bbox.coords[n_dim2 + j] = bbox.coords[n_dim2 + j].max(cell_coords[n_dim2 + j]);
            }
        }
        bbox
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

    fn bbox_union(&mut self, other: &RtreeCell, n_dim2: usize) {
        for i in 0..n_dim2 {
            self.coords[i] = self.coords[i].min(other.coords[i]);
            self.coords[n_dim2 + i] = self.coords[n_dim2 + i].max(other.coords[n_dim2 + i]);
        }
    }
}

struct RtreeSearchIter<'a> {
    table: &'a RtreeTable,
    conn: &'a Arc<Connection>,
    constraints: &'a [RtreeConstraint],
    point_queue: Vec<RtreeSearchPoint>,
    current_rowid: i64,
    at_eof: bool,
}

impl<'a> RtreeSearchIter<'a> {
    fn push_point(&mut self, id: i64, i_level: u8, e_within: u8, i_cell: u8) {
        self.point_queue.push(RtreeSearchPoint {
            r_score: 0.0,
            id,
            i_level,
            e_within,
            i_cell,
        });
    }

    fn sort_points(&mut self) {
        self.point_queue.sort_by(|a, b| {
            b.r_score
                .partial_cmp(&a.r_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    fn next_point(&mut self) -> Option<RtreeSearchPoint> {
        self.point_queue.pop()
    }
}

#[derive(Debug)]
struct RtreeCursor {
    at_eof: bool,
    rowid: i64,
    current_coords: [f32; RTREE_MAX_DIMENSIONS * 2],
    constraints: Vec<RtreeConstraint>,
    points: Vec<RtreeSearchPoint>,
    s_point: RtreeSearchPoint,
    conn: Option<Arc<Connection>>,
    n_dim2: usize,
    n_bytes_per_cell: usize,
    node_size: usize,
    table_name: String,
    nodes: [Option<Arc<RtreeNode>>; RTREE_CACHE_SZ],
}

impl RtreeCursor {
    fn new(
        conn: Option<Arc<Connection>>,
        n_dim2: usize,
        n_bytes_per_cell: usize,
        node_size: usize,
        table_name: String,
    ) -> Self {
        RtreeCursor {
            at_eof: true,
            rowid: 0,
            current_coords: [0.0; RTREE_MAX_DIMENSIONS * 2],
            constraints: Vec::new(),
            points: Vec::new(),
            s_point: RtreeSearchPoint::default(),
            conn,
            n_dim2,
            n_bytes_per_cell,
            node_size,
            table_name,
            nodes: [const { None }; RTREE_CACHE_SZ],
        }
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
                        parent: None,
                        n_ref: 1,
                        is_dirty: false,
                    });
                }
            }
        }
        None
    }

    fn next_search_point(&mut self) -> Option<RtreeSearchPoint> {
        self.points.pop()
    }

    fn sort_points(&mut self) {
        self.points.sort_by(|a, b| {
            a.r_score
                .partial_cmp(&b.r_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
}

#[derive(Debug, Default, Clone)]
struct RtreeSearchPoint {
    r_score: f64,
    id: i64,
    i_level: u8,
    e_within: u8,
    i_cell: u8,
}

#[derive(Debug)]
struct RtreeConstraint {
    i_coord: usize,
    op: u8,
    value: f64,
}

fn cell_intersects_query(cell: &RtreeCell, constraints: &[RtreeConstraint], n_dim2: usize) -> bool {
    for cons in constraints {
        let coord_idx = cons.i_coord;
        if coord_idx >= n_dim2 * 2 {
            continue;
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
        let val = cons.value as f32;
        match cons.op {
            b'A' => {
                if val != coord_min && val != coord_max {
                    return false;
                }
            }
            b'B' => {
                if val < coord_min {
                    return false;
                }
            }
            b'C' => {
                if val <= coord_max {
                    return false;
                }
            }
            b'D' => {
                if val <= coord_max {
                    return false;
                }
            }
            b'E' => {
                if val >= coord_min {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

fn leaf_constraint(pConstraint: &RtreeConstraint, cell: &RtreeCell, n_dim2: usize) -> i32 {
    let coord_idx = pConstraint.i_coord;
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

    let val = pConstraint.value as f32;
    match pConstraint.op {
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

fn nonleaf_constraint(pConstraint: &RtreeConstraint, cell: &RtreeCell, n_dim2: usize) -> i32 {
    let coord_idx = pConstraint.i_coord;
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

    let val = pConstraint.value as f32;
    match pConstraint.op {
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

fn compute_r_score(cell: &RtreeCell, constraints: &[RtreeConstraint], n_dim2: usize) -> f64 {
    let mut r_score = 0.0;
    for cons in constraints {
        let coord_idx = cons.i_coord;
        if coord_idx >= n_dim2 * 2 {
            continue;
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
        let val = cons.value as f32;
        match cons.op {
            b'B' => {
                if val > coord_min {
                    r_score += (val - coord_min) as f64;
                }
            }
            b'D' => {
                if val <= coord_max {
                    r_score += (coord_max - val) as f64;
                }
            }
            _ => {}
        }
    }
    r_score
}

fn choose_leaf(
    table: &RtreeTable,
    conn: Option<&Arc<Connection>>,
    _cell: &RtreeCell,
    n_dim2: usize,
) -> Option<RtreeNode> {
    let root = match conn {
        Some(c) => table.load_root_node(c).ok()?,
        None => RtreeNode::new(1, 0, table.node_size),
    };
    let mut current = root;
    let mut depth = current.tree_depth();

    while depth > 1 {
        let n_cells = current.cell_count();
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
    }

    Some(current)
}

fn split_node(
    table: &RtreeTable,
    node: &RtreeNode,
    cell: &RtreeCell,
    n_dim2: usize,
) -> (RtreeNode, RtreeNode) {
    let mut left_node = RtreeNode::new(0, node.depth, table.node_size);
    let mut right_node = RtreeNode::new(0, node.depth, table.node_size);

    let n_cells = node.cell_count();
    let mut cells: Vec<RtreeCell> = (0..n_cells)
        .map(|i| node.get_cell(n_dim2, table.n_bytes_per_cell, i))
        .collect();
    cells.push(cell.clone());

    let n_total = cells.len();
    let mid = (n_total + 1) / 2;

    let mut left_min = [f32::MAX; RTREE_MAX_DIMENSIONS];
    let mut left_max = [f32::MIN; RTREE_MAX_DIMENSIONS];
    let mut right_min = [f32::MAX; RTREE_MAX_DIMENSIONS];
    let mut right_max = [f32::MIN; RTREE_MAX_DIMENSIONS];

    for i in 0..mid {
        for j in 0..n_dim2 {
            left_min[j] = left_min[j].min(cells[i].coords[j]);
            left_min[j] = left_min[j].min(cells[i].coords[n_dim2 + j]);
            left_max[j] = left_max[j].max(cells[i].coords[j]);
            left_max[j] = left_max[j].max(cells[i].coords[n_dim2 + j]);
        }
    }

    for i in mid..n_total {
        for j in 0..n_dim2 {
            right_min[j] = right_min[j].min(cells[i].coords[j]);
            right_min[j] = right_min[j].min(cells[i].coords[n_dim2 + j]);
            right_max[j] = right_max[j].max(cells[i].coords[j]);
            right_max[j] = right_max[j].max(cells[i].coords[n_dim2 + j]);
        }
    }

    let mut left_count = 0;
    let mut right_count = 0;

    for (i, c) in cells.iter().enumerate() {
        if i < mid {
            left_node.set_cell(n_dim2, table.n_bytes_per_cell, left_count, c);
            left_count += 1;
        } else {
            right_node.set_cell(n_dim2, table.n_bytes_per_cell, right_count, c);
            right_count += 1;
        }
    }

    left_node.set_cell_count(left_count);
    right_node.set_cell_count(right_count);

    (left_node, right_node)
}

fn rtree_step_to_leaf(cursor: &mut RtreeCursor, table: &RtreeTable) -> ResultCode {
    while let Some(p) = cursor.next_search_point() {
        if p.i_level == 0 {
            continue;
        }

        let node = cursor.nodes[0].as_ref();
        let Some(node) = node else {
            continue;
        };

        let n_cell = node.cell_count();
        let n_bytes_per_cell = table.n_bytes_per_cell;
        let n_dim2 = table.n_dim2;

        let mut p = p;
        let mut found = false;

        while (p.i_cell as usize) < n_cell {
            let cell = node.get_cell(n_dim2, n_bytes_per_cell, p.i_cell as usize);
            let mut e_within = FULLY_WITHIN;

            for constraint in &cursor.constraints {
                let result = if p.i_level == 1 {
                    leaf_constraint(constraint, &cell, n_dim2)
                } else {
                    nonleaf_constraint(constraint, &cell, n_dim2)
                };

                if result == NOT_WITHIN {
                    e_within = NOT_WITHIN;
                    break;
                }
            }

            if e_within != NOT_WITHIN {
                let r_score = compute_r_score(&cell, &cursor.constraints, n_dim2);
                let new_point = RtreeSearchPoint {
                    r_score,
                    id: p.id,
                    i_level: p.i_level,
                    e_within: e_within as u8,
                    i_cell: p.i_cell,
                };
                cursor.points.push(new_point);
                cursor.sort_points();
                found = true;
                break;
            }

            p.i_cell += 1;
        }

        if found {
            break;
        }
    }

    if let Some(next_point) = cursor.next_search_point() {
        cursor.s_point = next_point;
        cursor.at_eof = false;
        ResultCode::OK
    } else {
        cursor.at_eof = true;
        ResultCode::EOF
    }
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

    fn insert(
        &mut self,
        conn: Option<Arc<Connection>>,
        args: &[Value],
    ) -> Result<i64, Self::Error> {
        let Some(conn) = conn else {
            return Err(ResultCode::Error);
        };
        // xUpdate passes argv[2..] as `columns`: id + coordinate pairs (see turso_macros vtab_derive).
        if args.len() < self.n_dim2 + 1 {
            return Err(ResultCode::Error);
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

        if self.node_count == 0 {
            self.create_shadow_tables(&conn)?;
        }

        let leaf_opt = choose_leaf(self, Some(&conn), &cell, self.n_dim2);
        let leaf_nodeno = if let Some(mut leaf) = leaf_opt {
            let n_cells = leaf.cell_count();
            let max_cells = self.max_cells();
            let nodeno = leaf.node_no;

            if n_cells < max_cells {
                leaf.set_cell(self.n_dim2, self.n_bytes_per_cell, n_cells, &cell);
                leaf.set_cell_count(n_cells + 1);
                self.write_node(&conn, nodeno, &leaf)?;
                nodeno
            } else {
                let n_total = n_cells + 1;
                let mid = (n_total + 1) / 2;
                let new_idx = n_total - 1;
                let (left, right) = split_node(self, &leaf, &cell, self.n_dim2);
                self.node_count += 2;
                let left_no = self.node_count - 1;
                let right_no = self.node_count;
                self.write_node(&conn, left_no, &left)?;
                self.write_node(&conn, right_no, &right)?;

                leaf.set_cell(
                    self.n_dim2,
                    self.n_bytes_per_cell,
                    0,
                    &RtreeCell::new(left_no),
                );
                leaf.set_cell_count(1);
                leaf.set_depth(leaf.tree_depth() + 1);
                self.write_node(&conn, 1, &leaf)?;
                if new_idx < mid {
                    left_no
                } else {
                    right_no
                }
            }
        } else {
            self.node_count += 1;
            let new_node_no = self.node_count;
            let mut new_node = RtreeNode::new(new_node_no, 0, self.node_size);
            new_node.set_cell(self.n_dim2, self.n_bytes_per_cell, 0, &cell);
            new_node.set_cell_count(1);
            self.write_node(&conn, new_node_no, &new_node)?;
            self.depth = 1;
            new_node_no
        };

        self.write_rowid_map(&conn, rowid, leaf_nodeno)?;

        Ok(rowid)
    }

    fn delete(&mut self, conn: Option<Arc<Connection>>, rowid: i64) -> Result<(), Self::Error> {
        let Some(conn) = conn else {
            return Err(ResultCode::Error);
        };
        if let Some(nodeno) = self.get_rowid_nodeno(&conn, rowid)? {
            if let Some(mut node) = self.read_node(&conn, nodeno)? {
                if let Some(cell_idx) = self.find_cell_index(&node, rowid) {
                    self.delete_cell_from_node(&mut node, cell_idx);
                    self.write_node(&conn, nodeno, &node)?;
                }
            }
        }
        self.delete_rowid_map(&conn, rowid)?;
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
        let mut estimated_rows = RTREE_DEFAULT_ROWEST as u32;
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
        self.points.clear();
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
        let coord_idx = (idx - 1) as usize;
        if coord_idx < self.current_coords.len() {
            Ok(Value::from_float(self.current_coords[coord_idx] as f64))
        } else {
            Ok(Value::null())
        }
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

    fn new_table(args: Vec<&str>) -> RtreeTable {
        let args = &args
            .iter()
            .map(|s| Value::from_text(s.to_string()))
            .collect::<Vec<_>>();
        RtreeModule::create(args).unwrap().1
    }

    #[test]
    fn test_create_table() {
        let table = new_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax",
        ]);
        assert_eq!(table.n_dim, 2);
        assert_eq!(table.n_dim2, 4);
        assert_eq!(table.e_coord_type, RTREE_COORD_REAL32);
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
        let cell = RtreeCell::new(1);
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
    fn test_compute_r_score() {
        let mut coords = [0.0; RTREE_MAX_DIMENSIONS * 2];
        coords[0] = 5.0;
        coords[1] = 10.0;
        let cell = RtreeCell::new(1);
        let mut test_cell = RtreeCell::new(1);
        test_cell.coords = coords;

        let constraints = vec![RtreeConstraint {
            i_coord: 0,
            op: b'B',
            value: 6.0,
        }];

        let score = compute_r_score(&test_cell, &constraints, 4);
        assert!(score >= 0.0);
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

    #[test]
    fn test_split_node() {
        let table = new_table(vec![
            "rtree", "main", "test", "id", "xmin", "xmax", "ymin", "ymax",
        ]);
        let mut node = RtreeNode::new(1, 0, 4096 - 64);
        node.set_cell_count(0);

        let cell = RtreeCell::new(1);
        let (left, right) = split_node(&table, &node, &cell, 4);

        assert!(left.cell_count() > 0 || right.cell_count() > 0);
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
}
