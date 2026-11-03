//! The `dbstat` virtual table: per-page b-tree space usage, as in SQLite's
//! `src/dbstat.c`. Row values, order and corruption handling follow that file.

use crate::native_ext::{
    ConnectInfo, ConstraintInfo, ConstraintUsage, IndexInfo, OrderByInfo, VirtualTable,
    VirtualTableCursor, VirtualTableModule,
};
use crate::numeric::Numeric;
use crate::storage::pager::Pager;
use crate::sync::Arc;
use crate::types::{IOResult, IOResultOr};
use crate::util::IOExt;
use crate::{Connection, LimboError, Result, Value};
use turso_ext::{ConstraintOp, ResultCode};

pub const DBSTAT_MODULE_NAME: &str = "dbstat";

const DBSTAT_SCHEMA: &str = "CREATE TABLE x( name       TEXT, path       TEXT, pageno     INTEGER, pagetype   TEXT, ncell      INTEGER, payload    INTEGER, unused     INTEGER, mx_payload INTEGER, pgoffset   INTEGER, pgsize     INTEGER, schema     TEXT HIDDEN, aggregate  BOOLEAN HIDDEN)";

const COL_NAME: u32 = 0;
const COL_SCHEMA: u32 = 10;
const COL_AGGREGATE: u32 = 11;
/// SQLite's limit on b-tree depth in dbstat.
const MAX_DEPTH: usize = 32;

/// The `dbstat` module. Register it with `Connection::register_vtab_module_local`
/// as creatable and eponymous.
#[derive(Debug)]
pub struct DbstatModule {
    /// Checks the schema argument when a table is created.
    conn: crate::sync::Weak<Connection>,
}

impl DbstatModule {
    pub fn new(conn: &Arc<Connection>) -> Self {
        Self {
            conn: Arc::downgrade(conn),
        }
    }
}

#[derive(Debug)]
pub struct DbstatTable {
    /// Schema named by `CREATE VIRTUAL TABLE x USING dbstat(schema)`.
    schema: Option<String>,
}

impl VirtualTableModule for DbstatModule {
    type Table = DbstatTable;

    fn schema(&self, _args: &[Value]) -> Result<String> {
        Ok(DBSTAT_SCHEMA.to_string())
    }

    fn create(&self, args: &[Value]) -> Result<DbstatTable> {
        let schema = args.first().and_then(|a| a.to_text()).map(dequote);
        Ok(DbstatTable { schema })
    }

    fn connect(&self, _info: &ConnectInfo, args: &[Value]) -> Result<(DbstatTable, String)> {
        let table = self.create(args)?;
        if let (Some(schema), Some(conn)) = (&table.schema, self.conn.upgrade()) {
            if resolve_schema(&conn, schema).is_none() {
                return Err(LimboError::SqlError(format!("no such database: {schema}")));
            }
        }
        Ok((table, DBSTAT_SCHEMA.to_string()))
    }
}

fn dequote(s: &str) -> String {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2 {
        let (q, end) = (b[0], b[b.len() - 1]);
        let close = match q {
            b'"' | b'\'' | b'`' => q,
            b'[' => b']',
            _ => 0,
        };
        if close != 0 && end == close {
            let inner = &s[1..s.len() - 1];
            return if q == b'[' {
                inner.to_string()
            } else {
                let qs = (q as char).to_string();
                inner.replace(&qs.repeat(2), &qs)
            };
        }
    }
    s.to_string()
}

impl VirtualTable for DbstatTable {
    type Cursor = DbstatCursor;

    fn open(&self, connection: Arc<Connection>) -> Result<DbstatCursor> {
        if let Some(schema) = &self.schema {
            if resolve_schema(&connection, schema).is_none() {
                return Err(LimboError::SqlError(format!("no such database: {schema}")));
            }
        }
        Ok(DbstatCursor {
            conn: connection,
            table_schema: self.schema.clone(),
            rows: Vec::new(),
            pos: 0,
            schema_name: "main".to_string(),
            is_agg: false,
        })
    }

    /// idxNum bits: 0x01 schema=?, 0x02 name=?, 0x04 aggregate=?, 0x08 ordered.
    fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> std::result::Result<IndexInfo, ResultCode> {
        let (mut i_schema, mut i_name, mut i_agg) = (None, None, None);
        for (i, c) in constraints.iter().enumerate() {
            if c.op != ConstraintOp::Eq {
                continue;
            }
            if !c.usable {
                // Keeps dbstat the right-most table in a join.
                return Err(ResultCode::ConstraintViolation);
            }
            match c.column_index {
                COL_NAME => i_name = Some(i),
                COL_SCHEMA => i_schema = Some(i),
                COL_AGGREGATE => i_agg = Some(i),
                _ => {}
            }
        }
        let mut usages = vec![
            ConstraintUsage {
                argv_index: None,
                omit: false,
            };
            constraints.len()
        ];
        let mut idx_num = 0;
        let mut argc = 0;
        if let Some(i) = i_schema {
            argc += 1;
            usages[i] = ConstraintUsage {
                argv_index: Some(argc),
                omit: true,
            };
            idx_num |= 0x01;
        }
        if let Some(i) = i_name {
            argc += 1;
            usages[i].argv_index = Some(argc);
            idx_num |= 0x02;
        }
        if let Some(i) = i_agg {
            argc += 1;
            usages[i].argv_index = Some(argc);
            idx_num |= 0x04;
        }
        let ordered = match order_by {
            [a] => a.column_index == 0 && !a.desc,
            [a, b] => a.column_index == 0 && !a.desc && b.column_index == 1 && !b.desc,
            _ => false,
        };
        if ordered {
            idx_num |= 0x08;
        }
        Ok(IndexInfo {
            idx_num,
            idx_str: None,
            order_by_consumed: ordered,
            estimated_cost: 1.0,
            estimated_rows: u32::MAX,
            constraint_usages: usages,
        })
    }
}

/// Resolves a schema name the way `sqlite3FindDbName` does; returns the
/// database index and its canonical name.
fn resolve_schema(conn: &Connection, name: &str) -> Option<(usize, String)> {
    if name.eq_ignore_ascii_case("main") {
        return Some((crate::MAIN_DB_ID, "main".to_string()));
    }
    if name.eq_ignore_ascii_case("temp") {
        return Some((crate::TEMP_DB_ID, "temp".to_string()));
    }
    let alias = conn
        .attached_database_names()
        .into_iter()
        .find(|a| a.eq_ignore_ascii_case(name))?;
    let id = conn.get_database_id_by_name(&alias).ok()?;
    Some((id, alias))
}

#[derive(Debug, Clone)]
struct StatRow {
    name: String,
    path: Option<String>,
    pageno: i64,
    pagetype: Option<&'static str>,
    ncell: i64,
    payload: i64,
    unused: i64,
    mx_payload: i64,
    pgoffset: Option<i64>,
    pgsize: i64,
    rowid: i64,
}

pub struct DbstatCursor {
    conn: Arc<Connection>,
    table_schema: Option<String>,
    rows: Vec<StatRow>,
    pos: usize,
    schema_name: String,
    is_agg: bool,
}

impl VirtualTableCursor for DbstatCursor {
    fn filter(&mut self, args: &[Value], _idx_str: Option<&str>, idx_num: i32) -> IOResultOr<bool> {
        self.rows.clear();
        self.pos = 0;
        let mut args = args.iter();
        let db = if idx_num & 0x01 != 0 {
            let name = args.next().map(value_text).unwrap_or_default();
            match resolve_schema(&self.conn, &name) {
                Some(db) => db,
                None => {
                    self.schema_name = "main".to_string();
                    return Ok(IOResult::Done(false));
                }
            }
        } else {
            let name = self.table_schema.as_deref().unwrap_or("main");
            resolve_schema(&self.conn, name)
                .ok_or_else(|| LimboError::SqlError(format!("no such database: {name}")))?
        };
        let name_filter = if idx_num & 0x02 != 0 {
            args.next().map(value_text)
        } else {
            None
        };
        self.is_agg = if idx_num & 0x04 != 0 {
            args.next().map(value_f64).unwrap_or(0.0) != 0.0
        } else {
            false
        };
        self.schema_name = db.1;
        let pager = self.conn.get_pager_from_database_index(&db.0)?;
        let mut walker = Walker::new(&pager)?;
        // SQLite sizes pages by the table's own schema, not the scanned one.
        let table_db = self.table_schema.as_deref().unwrap_or("main");
        if let Some((id, _)) = resolve_schema(&self.conn, table_db) {
            if id != db.0 {
                let p = self.conn.get_pager_from_database_index(&id)?;
                walker.size_page = p.usable_space() + p.get_reserved_space().unwrap_or(0) as usize;
            }
        }
        let Some(page_count) = walker.page_count()? else {
            return Ok(IOResult::Done(false));
        };
        if page_count == 0 {
            return Ok(IOResult::Done(false));
        }
        let mut roots = walker.schema_roots()?;
        if let Some(name) = &name_filter {
            roots.retain(|(n, _)| n == name);
        }
        if idx_num & 0x08 != 0 {
            roots.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        }
        for (name, root) in roots {
            walker.analyze(&name, root, self.is_agg, &mut self.rows)?;
        }
        Ok(IOResult::Done(!self.rows.is_empty()))
    }

    fn next(&mut self) -> IOResultOr<bool> {
        self.pos += 1;
        Ok(IOResult::Done(self.pos < self.rows.len()))
    }

    fn column(&mut self, column: usize) -> IOResultOr<Value> {
        let agg = self.is_agg;
        if column == 10 {
            return Ok(IOResult::Done(Value::from_text(self.schema_name.clone())));
        }
        if column == 11 {
            return Ok(IOResult::Done(Value::from_i64(agg as i64)));
        }
        let Some(row) = self.rows.get(self.pos) else {
            return Ok(IOResult::Done(Value::Null));
        };
        let opt_text = |t: Option<String>| t.map(Value::from_text).unwrap_or(Value::Null);
        let v = match column {
            0 => Value::from_text(row.name.clone()),
            1 => opt_text(row.path.clone()),
            2 => Value::from_i64(row.pageno),
            3 => opt_text(row.pagetype.map(str::to_string)),
            4 => Value::from_i64(row.ncell),
            5 => Value::from_i64(row.payload),
            6 => Value::from_i64(row.unused),
            7 => Value::from_i64(row.mx_payload),
            8 => row.pgoffset.map(Value::from_i64).unwrap_or(Value::Null),
            9 => Value::from_i64(row.pgsize),
            _ => Value::Null,
        };
        Ok(IOResult::Done(v))
    }

    fn rowid(&self) -> i64 {
        self.rows.get(self.pos).map(|r| r.rowid).unwrap_or(0)
    }
}

/// `sqlite3_value_text` of a constraint value.
fn value_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Text(t) => t.as_str().to_string(),
        Value::Blob(b) => String::from_utf8_lossy(b.as_slice()).into_owned(),
        other => other.to_string(),
    }
}

/// `sqlite3_value_double` of a constraint value.
fn value_f64(v: &Value) -> f64 {
    match v {
        Value::Numeric(Numeric::Integer(i)) => *i as f64,
        Value::Numeric(Numeric::Float(f)) => f64::from(*f),
        Value::Text(t) => crate::numeric::str_to_f64(t.as_str())
            .map(f64::from)
            .unwrap_or(0.0),
        Value::Blob(b) => crate::numeric::str_to_f64(String::from_utf8_lossy(b.as_slice()))
            .map(f64::from)
            .unwrap_or(0.0),
        Value::Null => 0.0,
    }
}

#[derive(Default, Clone)]
struct StatCell {
    n_local: i64,
    child_pg: u32,
    ovfl: Vec<u32>,
    n_last_ovfl: i64,
    i_ovfl: usize,
}

#[derive(Default, Clone)]
struct StatPage {
    pgno: u32,
    data: Vec<u8>,
    i_cell: usize,
    path: String,
    flags: u8,
    n_cell: usize,
    n_unused: i64,
    cells: Vec<StatCell>,
    right_child: u32,
    mx_payload: i64,
}

/// Running counters for one output row (or one b-tree when aggregating).
#[derive(Default)]
struct Counts {
    pageno: u32,
    n_page: i64,
    n_cell: i64,
    mx_payload: i64,
    unused: i64,
    payload: i64,
    offset: i64,
    sz_page: i64,
}

struct Walker<'a> {
    pager: &'a Arc<Pager>,
    page_size: usize,
    usable: usize,
    /// Page size reported in pgsize and pgoffset.
    size_page: usize,
}

fn get2(a: &[u8], off: usize) -> usize {
    match a.get(off..off + 2) {
        Some(b) => ((b[0] as usize) << 8) | b[1] as usize,
        None => 0,
    }
}

fn get4(a: &[u8], off: usize) -> u32 {
    match a.get(off..off + 4) {
        Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

/// SQLite varint; returns (value, bytes read). Reads past the end as zero,
/// like SQLite's padded page buffers.
fn get_varint(a: &[u8], off: usize) -> (u64, usize) {
    let byte = |i: usize| a.get(off + i).copied().unwrap_or(0);
    let mut v: u64 = 0;
    for i in 0..8 {
        let b = byte(i);
        v = (v << 7) | (b & 0x7f) as u64;
        if b & 0x80 == 0 {
            return (v, i + 1);
        }
    }
    v = (v << 8) | byte(8) as u64;
    (v, 9)
}

/// `getVarint32`: values that do not fit in 32 bits become 0xffffffff.
fn get_varint32(a: &[u8], off: usize) -> (u32, usize) {
    let (v, n) = get_varint(a, off);
    (
        if v > u32::MAX as u64 {
            u32::MAX
        } else {
            v as u32
        },
        n,
    )
}

fn local_payload(usable: i64, flags: u8, total: i64) -> i64 {
    let min_local = (usable - 12) * 32 / 255 - 23;
    let max_local = if flags == 0x0D {
        usable - 35
    } else {
        (usable - 12) * 64 / 255 - 23
    };
    let n_local = min_local + (total - min_local) % (usable - 4);
    if n_local > max_local {
        min_local
    } else {
        n_local
    }
}

impl<'a> Walker<'a> {
    fn new(pager: &'a Arc<Pager>) -> Result<Self> {
        let usable = pager.usable_space();
        let reserved = pager.get_reserved_space().unwrap_or(0) as usize;
        Ok(Self {
            pager,
            page_size: usable + reserved,
            usable,
            size_page: usable + reserved,
        })
    }

    /// Database size in pages, or None for a database with no file yet.
    fn page_count(&self) -> Result<Option<u32>> {
        if !self.pager.db_initialized() {
            return Ok(None);
        }
        let n = self
            .pager
            .io
            .block(|| self.pager.with_header(|h| h.database_size.get()))?;
        Ok(Some(n))
    }

    fn read_page(&self, pgno: u32) -> Result<Vec<u8>> {
        if pgno == 0 {
            return Err(LimboError::Corrupt("dbstat: page 0".to_string()));
        }
        let (page, completion) = self.pager.io.block(|| self.pager.read_page(pgno as i64))?;
        if let Some(c) = completion {
            self.pager.io.wait_for_completion(c)?;
        }
        let contents = page.get_contents();
        let raw = contents.as_ptr();
        let mut data = vec![0u8; self.page_size];
        let n = raw.len().min(self.page_size);
        data[..n].copy_from_slice(&raw[..n]);
        Ok(data)
    }

    /// (name, rootpage) of 'sqlite_schema' and each sqlite_schema row with a
    /// non-zero rootpage, in rowid order.
    fn schema_roots(&self) -> Result<Vec<(String, u32)>> {
        let mut out = vec![("sqlite_schema".to_string(), 1u32)];
        let mut stack = vec![(1u32, 0usize)];
        while let Some((pgno, depth)) = stack.pop() {
            if depth > MAX_DEPTH {
                return Err(LimboError::Corrupt("dbstat: schema too deep".to_string()));
            }
            let data = self.read_page(pgno)?;
            let hdr = if pgno == 1 { 100 } else { 0 };
            let flags = data[hdr];
            let n_cell = get2(&data, hdr + 3);
            match flags {
                0x05 => {
                    let cells = hdr + 12;
                    stack.push((get4(&data, hdr + 8), depth + 1));
                    for i in (0..n_cell).rev() {
                        let off = get2(&data, cells + i * 2);
                        stack.push((get4(&data, off), depth + 1));
                    }
                }
                0x0D => {
                    let cells = hdr + 8;
                    for i in 0..n_cell {
                        let off = get2(&data, cells + i * 2);
                        let payload = self.cell_payload(&data, off)?;
                        if let Some(row) = schema_row(&payload) {
                            out.push(row);
                        }
                    }
                }
                _ => return Err(LimboError::Corrupt("dbstat: bad schema page".to_string())),
            }
        }
        Ok(out)
    }

    /// Full payload of a table leaf cell, following its overflow chain.
    fn cell_payload(&self, data: &[u8], mut off: usize) -> Result<Vec<u8>> {
        let (total, n) = get_varint(data, off);
        off += n;
        off += get_varint(data, off).1;
        let total = total as usize;
        let usable = self.usable as i64;
        let max_local = (usable - 35) as usize;
        let n_local = if total <= max_local {
            total
        } else {
            local_payload(usable, 0x0D, total as i64) as usize
        };
        let mut out = data
            .get(off..off + n_local)
            .ok_or_else(|| LimboError::Corrupt("dbstat: cell out of page".to_string()))?
            .to_vec();
        let mut next = if total > n_local {
            get4(data, off + n_local)
        } else {
            0
        };
        while out.len() < total && next != 0 {
            let page = self.read_page(next)?;
            next = get4(&page, 0);
            let take = (total - out.len()).min(self.usable - 4);
            out.extend_from_slice(&page[4..4 + take]);
        }
        Ok(out)
    }

    fn get_page(&self, pgno: u32, page: &mut StatPage) -> Result<()> {
        page.data = self.read_page(pgno)?;
        Ok(())
    }

    /// `statDecodePage`.
    fn decode_page(&self, p: &mut StatPage) -> Result<()> {
        let hdr = if p.pgno == 1 { 100 } else { 0 };
        let usable = self.usable as i64;
        let sz_page = self.page_size;
        let corrupt = |p: &mut StatPage| {
            p.flags = 0;
            p.cells.clear();
            p.n_cell = 0;
        };
        p.flags = p.data[hdr];
        let (is_leaf, mut n_hdr) = match p.flags {
            0x0A | 0x0D => (true, 8),
            0x05 | 0x02 => (false, 12),
            _ => {
                corrupt(p);
                return Ok(());
            }
        };
        if p.pgno == 1 {
            n_hdr += 100;
        }
        p.n_cell = get2(&p.data, hdr + 3);
        p.mx_payload = 0;
        let mut n_unused = get2(&p.data, hdr + 5) as i64 - n_hdr as i64 - 2 * p.n_cell as i64;
        n_unused += p.data[hdr + 7] as i64;
        let mut off = get2(&p.data, hdr + 1);
        while off != 0 {
            if off >= sz_page {
                corrupt(p);
                return Ok(());
            }
            n_unused += get2(&p.data, off + 2) as i64;
            let next = get2(&p.data, off);
            if next < off + 4 && next > 0 {
                corrupt(p);
                return Ok(());
            }
            off = next;
        }
        p.n_unused = n_unused;
        p.right_child = if is_leaf { 0 } else { get4(&p.data, hdr + 8) };

        if p.n_cell == 0 {
            return Ok(());
        }
        p.cells = vec![StatCell::default(); p.n_cell + 1];
        for i in 0..p.n_cell {
            let mut off = get2(&p.data, n_hdr + i * 2);
            if off < n_hdr || off >= sz_page {
                corrupt(p);
                return Ok(());
            }
            if !is_leaf {
                p.cells[i].child_pg = get4(&p.data, off);
                off += 4;
            }
            if p.flags == 0x05 {
                continue;
            }
            let (n_payload, n) = get_varint32(&p.data, off);
            off += n;
            if p.flags == 0x0D {
                off += get_varint(&p.data, off).1;
            }
            if n_payload as i64 > p.mx_payload {
                p.mx_payload = n_payload as i64;
            }
            // SQLite passes the u32 payload size as an int.
            let n_local = local_payload(usable, p.flags, n_payload as i32 as i64);
            if n_local < 0 {
                corrupt(p);
                return Ok(());
            }
            p.cells[i].n_local = n_local;
            if n_payload as i64 > n_local {
                let n_ovfl = ((n_payload as i64 - n_local) + usable - 4 - 1) / (usable - 4);
                if off as i64 + n_local + 4 > usable || n_payload > 0x7fff_ffff {
                    corrupt(p);
                    return Ok(());
                }
                p.cells[i].n_last_ovfl = (n_payload as i64 - n_local) - (n_ovfl - 1) * (usable - 4);
                let mut ovfl = Vec::with_capacity(n_ovfl as usize);
                ovfl.push(get4(&p.data, off + n_local as usize));
                for j in 1..n_ovfl as usize {
                    let prev = self.read_page(ovfl[j - 1])?;
                    ovfl.push(get4(&prev, 0));
                }
                p.cells[i].ovfl = ovfl;
            }
        }
        Ok(())
    }

    fn size_and_offset(&self, c: &mut Counts) {
        c.sz_page += self.size_page as i64;
        c.offset = c.sz_page * (c.pageno as i64 - 1);
    }

    /// Emits the rows for one b-tree, porting `statNext`.
    fn analyze(&self, name: &str, root: u32, is_agg: bool, out: &mut Vec<StatRow>) -> Result<()> {
        let mut pages: Vec<StatPage> = vec![StatPage::default(); MAX_DEPTH];
        let mut c = Counts::default();
        let emit = |c: &Counts, path: Option<String>, pagetype: Option<&'static str>| StatRow {
            name: name.to_string(),
            path,
            pageno: if is_agg { c.n_page } else { c.pageno as i64 },
            pagetype,
            ncell: c.n_cell,
            payload: c.payload,
            unused: c.unused,
            mx_payload: c.mx_payload,
            pgoffset: (!is_agg).then_some(c.offset),
            pgsize: c.sz_page,
            rowid: c.pageno as i64,
        };

        self.get_page(root, &mut pages[0])?;
        pages[0].pgno = root;
        pages[0].i_cell = 0;
        pages[0].path = "/".to_string();
        let mut i_page: isize = 0;
        c.n_page = 1;
        let usable = self.usable as i64;

        // `fresh` is true when pages[i_page] was just loaded and must be decoded.
        let mut fresh = true;
        loop {
            if !fresh {
                let ip = i_page as usize;
                if !is_agg {
                    reset_counts(&mut c);
                }
                let mut emitted = None;
                {
                    let p = &mut pages[ip];
                    'cells: while p.i_cell < p.n_cell {
                        let i_cell = p.i_cell;
                        let cell = &mut p.cells[i_cell];
                        while cell.i_ovfl < cell.ovfl.len() {
                            let i_ovfl = cell.i_ovfl;
                            c.n_page += 1;
                            c.pageno = cell.ovfl[i_ovfl];
                            self.size_and_offset(&mut c);
                            if i_ovfl < cell.ovfl.len() - 1 {
                                c.payload += usable - 4;
                            } else {
                                c.payload += cell.n_last_ovfl;
                                c.unused += usable - 4 - cell.n_last_ovfl;
                            }
                            cell.i_ovfl += 1;
                            if !is_agg {
                                emitted = Some(format!("{}{:03x}+{:06x}", p.path, i_cell, i_ovfl));
                                break 'cells;
                            }
                        }
                        if p.right_child != 0 {
                            break;
                        }
                        p.i_cell += 1;
                    }
                }
                if let Some(path) = emitted {
                    out.push(emit(&c, Some(path), Some("overflow")));
                    continue;
                }
                let p = &pages[ip];
                if p.right_child == 0 || p.i_cell > p.n_cell {
                    pages[ip] = StatPage::default();
                    i_page -= 1;
                    if i_page < 0 {
                        if is_agg {
                            out.push(emit(&c, None, None));
                        }
                        return Ok(());
                    }
                    continue;
                }
                if ip + 1 >= MAX_DEPTH {
                    return Err(LimboError::Corrupt("dbstat: b-tree too deep".to_string()));
                }
                let child = if p.i_cell == p.n_cell {
                    p.right_child
                } else {
                    p.cells[p.i_cell].child_pg
                };
                let path = format!("{}{:03x}/", p.path, p.i_cell);
                pages[ip].i_cell += 1;
                i_page += 1;
                let np = &mut pages[ip + 1];
                *np = StatPage::default();
                np.pgno = child;
                self.get_page(child, np)?;
                np.path = path;
                c.n_page += 1;
            }
            fresh = false;

            // Populate the counters from the current page.
            let p = &mut pages[i_page as usize];
            c.pageno = p.pgno;
            self.decode_page(p)?;
            self.size_and_offset(&mut c);
            let pagetype = match p.flags {
                0x05 | 0x02 => "internal",
                0x0D | 0x0A => "leaf",
                _ => "corrupted",
            };
            c.n_cell += p.n_cell as i64;
            c.unused += p.n_unused;
            if p.mx_payload > c.mx_payload {
                c.mx_payload = p.mx_payload;
            }
            c.payload += p
                .cells
                .iter()
                .take(p.n_cell)
                .map(|x| x.n_local)
                .sum::<i64>();
            if !is_agg {
                out.push(emit(&c, Some(p.path.clone()), Some(pagetype)));
            }
        }
    }
}

fn reset_counts(c: &mut Counts) {
    c.n_cell = 0;
    c.mx_payload = 0;
    c.unused = 0;
    c.payload = 0;
    c.sz_page = 0;
    c.n_page = 0;
}

/// (name, rootpage) from a sqlite_schema record when rootpage != 0.
fn schema_row(rec: &[u8]) -> Option<(String, u32)> {
    let (hdr_len, mut hoff) = get_varint(rec, 0);
    let hdr_len = hdr_len as usize;
    let mut types = Vec::with_capacity(5);
    while hoff < hdr_len && types.len() < 4 {
        let (t, n) = get_varint(rec, hoff);
        hoff += n;
        types.push(t);
    }
    let mut off = hdr_len;
    let mut name = None;
    let mut root: i64 = 0;
    for (i, t) in types.iter().enumerate() {
        let t = *t;
        let len = match t {
            0 | 8 | 9 => 0,
            1..=4 => t as usize,
            5 => 6,
            6 | 7 => 8,
            10 | 11 => 0,
            _ => ((t - 12) / 2) as usize,
        };
        let bytes = rec.get(off..off + len)?;
        match i {
            1 if t >= 13 && t % 2 == 1 => name = Some(String::from_utf8_lossy(bytes).into_owned()),
            1 if t >= 12 => name = Some(String::from_utf8_lossy(bytes).into_owned()),
            3 => {
                root = match t {
                    1..=6 => {
                        let mut v: i64 = if bytes[0] & 0x80 != 0 { -1 } else { 0 };
                        for b in bytes {
                            v = (v << 8) | *b as i64;
                        }
                        v
                    }
                    7 => f64::from_be_bytes(bytes.try_into().ok()?) as i64,
                    8 => 0,
                    9 => 1,
                    // Text or blob rootpage: integer prefix, as sqlite3_column_int64.
                    _ if t >= 12 => {
                        crate::numeric::str_to_i64(String::from_utf8_lossy(bytes)).unwrap_or(0)
                    }
                    _ => 0,
                };
            }
            _ => {}
        }
        off += len;
    }
    // `rootpage != 0` is false for NULL.
    if types.get(3).is_none_or(|t| *t == 0) || root == 0 {
        return None;
    }
    Some((name.unwrap_or_default(), root as u32))
}
