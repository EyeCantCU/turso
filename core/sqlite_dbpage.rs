//! The `sqlite_dbpage` virtual table: whole raw pages of a database file,
//! readable and writable, as in SQLite's `src/dbpage.c`.

use crate::native_ext::{
    index_op, ConnectInfo, IndexInfo, IndexPlan, IndexRequest, TableUpdate, VirtualTable,
    VirtualTableCursor, VirtualTableModule, INDEX_SCAN_UNIQUE,
};
use crate::numeric::Numeric;
use crate::storage::pager::Pager;
use crate::sync::Arc;
use crate::types::{IOResult, IOResultOr};
use crate::util::IOExt;
use crate::{Connection, LimboError, Result, Value};
use turso_ext::{ConstraintInfo, ConstraintUsage, OrderByInfo, ResultCode};

pub const DBPAGE_MODULE_NAME: &str = "sqlite_dbpage";

const DBPAGE_SCHEMA: &str = "CREATE TABLE x(pgno INTEGER PRIMARY KEY, data BLOB, schema HIDDEN)";
const COL_SCHEMA: i32 = 2;

/// Register with `Connection::register_vtab_module_local` as creatable and
/// eponymous.
#[derive(Debug, Default)]
pub struct DbpageModule;

#[derive(Debug)]
pub struct DbpageTable;

impl VirtualTableModule for DbpageModule {
    type Table = DbpageTable;

    fn schema(&self, _args: &[Value]) -> Result<String> {
        Ok(DBPAGE_SCHEMA.to_string())
    }

    fn create(&self, _args: &[Value]) -> Result<DbpageTable> {
        Ok(DbpageTable)
    }

    fn connect(&self, _info: &ConnectInfo, _args: &[Value]) -> Result<(DbpageTable, String)> {
        Ok((DbpageTable, DBPAGE_SCHEMA.to_string()))
    }
}

/// dbpageBeginTrans: a write to sqlite_dbpage may touch any database, so
/// the statement opens a write transaction on all of them.
pub(crate) fn begin_writes(
    program: &mut crate::vdbe::builder::ProgramBuilder,
    resolver: &crate::translate::emitter::Resolver,
    connection: &Connection,
    table: &crate::schema::Table,
) -> Result<()> {
    let Some(vt) = table.virtual_table() else {
        return Ok(());
    };
    if !vt.name.eq_ignore_ascii_case(DBPAGE_MODULE_NAME) {
        return Ok(());
    }
    for (id, _, _) in connection.list_all_databases() {
        let cookie = resolver.with_schema(id, |s| s.schema_version);
        program
            .begin_write_on_database(id, cookie)
            .map_err(|e| LimboError::InternalError(e.to_string()))?;
    }
    Ok(())
}

/// `sqlite3FindDbName`: database index and canonical name.
pub(crate) fn find_db(conn: &Connection, name: &str) -> Option<(usize, String)> {
    conn.list_all_databases()
        .into_iter()
        .find(|(_, n, _)| n.eq_ignore_ascii_case(name))
        .map(|(id, n, _)| (id, n))
}

/// `sqlite3_value_int64`.
pub(crate) fn value_i64(v: &Value) -> i64 {
    match v {
        Value::Numeric(Numeric::Integer(i)) => *i,
        Value::Numeric(Numeric::Float(f)) => f64::from(*f) as i64,
        Value::Text(t) => crate::numeric::str_to_i64(t.as_str()).unwrap_or(0),
        Value::Blob(b) => {
            crate::numeric::str_to_i64(String::from_utf8_lossy(b.as_slice())).unwrap_or(0)
        }
        Value::Null => 0,
    }
}

/// `sqlite3_value_text`.
pub(crate) fn value_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Text(t) => t.as_str().to_string(),
        Value::Blob(b) => String::from_utf8_lossy(b.as_slice()).into_owned(),
        other => other.to_string(),
    }
}

fn page_size(pager: &Pager) -> usize {
    pager.usable_space() + pager.get_reserved_space().unwrap_or(0) as usize
}

/// `sqlite3BtreeLastPage`: 0 for a database with no file content yet.
fn last_page(pager: &Pager) -> Result<u32> {
    if !pager.db_initialized() {
        return Ok(0);
    }
    pager
        .io
        .block(|| pager.with_header(|h| h.database_size.get()))
}

fn read_page(pager: &Arc<Pager>, pgno: u32) -> Result<Vec<u8>> {
    let size = page_size(pager);
    if pager.pending_byte_page_id() == Some(pgno) {
        // SQLite assumes the pending-byte page is zeroed.
        return Ok(vec![0; size]);
    }
    let (page, c) = pager.io.block(|| pager.read_page(pgno as i64))?;
    if let Some(c) = c {
        pager.io.wait_for_completion(c)?;
    }
    let raw = page.get_contents().as_ptr();
    let mut data = vec![0u8; size];
    let n = raw.len().min(size);
    data[..n].copy_from_slice(&raw[..n]);
    Ok(data)
}

impl VirtualTable for DbpageTable {
    type Cursor = DbpageCursor;

    fn open(&self, connection: Arc<Connection>) -> Result<DbpageCursor> {
        Ok(DbpageCursor {
            conn: connection,
            pager: None,
            pgno: 1,
            mx_pgno: 0,
            schema: "main".to_string(),
        })
    }

    fn best_index(
        &self,
        _constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> std::result::Result<IndexInfo, ResultCode> {
        Err(ResultCode::Error)
    }

    /// idxNum: 0 main, full scan; 1 main, pgno=?1; 2 schema=?1, full scan;
    /// 3 schema=?1, pgno=?2.
    fn best_index_plan(&self, req: &IndexRequest) -> Result<Option<IndexPlan>> {
        let mut plan = IndexPlan::new(req.constraints.len());
        let mut i_plan = 0;
        for (i, c) in req.constraints.iter().enumerate() {
            if c.column != COL_SCHEMA || c.op != index_op::EQ {
                continue;
            }
            if !c.usable {
                return Ok(None);
            }
            i_plan = 2;
            plan.usages[i] = ConstraintUsage {
                argv_index: Some(1),
                omit: true,
            };
            break;
        }
        plan.estimated_cost = 1.0e6;
        for (i, c) in req.constraints.iter().enumerate() {
            if c.usable && c.column <= 0 && c.op == index_op::EQ {
                plan.estimated_rows = 1;
                plan.idx_flags = INDEX_SCAN_UNIQUE;
                plan.estimated_cost = 1.0;
                plan.usages[i] = ConstraintUsage {
                    argv_index: Some(if i_plan != 0 { 2 } else { 1 }),
                    omit: true,
                };
                i_plan |= 1;
                break;
            }
        }
        plan.idx_num = i_plan;
        if req
            .order_by
            .first()
            .is_some_and(|o| o.column <= 0 && !o.desc)
        {
            plan.order_by_consumed = true;
        }
        Ok(Some(plan))
    }

    fn readonly(&self) -> bool {
        false
    }

    fn create_update_with_connection(
        &mut self,
        connection: &Arc<Connection>,
        args: Vec<Value>,
        _on_conflict: u8,
    ) -> Result<Box<dyn TableUpdate>> {
        Ok(Box::new(DbpageUpdate {
            conn: connection.clone(),
            args,
        }))
    }
}

pub struct DbpageCursor {
    conn: Arc<Connection>,
    pager: Option<Arc<Pager>>,
    pgno: u32,
    mx_pgno: u32,
    schema: String,
}

impl VirtualTableCursor for DbpageCursor {
    fn filter(&mut self, args: &[Value], _idx_str: Option<&str>, idx_num: i32) -> IOResultOr<bool> {
        self.pgno = 1;
        self.mx_pgno = 0;
        let db = if idx_num & 2 != 0 {
            match find_db(
                &self.conn,
                &args.first().map(value_text).unwrap_or_default(),
            ) {
                Some(db) => db,
                None => return Ok(IOResult::Done(false)),
            }
        } else {
            (crate::MAIN_DB_ID, "main".to_string())
        };
        let pager = self.conn.get_pager_from_database_index(&db.0)?;
        self.schema = db.1;
        self.mx_pgno = last_page(&pager)?;
        if idx_num & 1 != 0 {
            let pg = args
                .get((idx_num >> 1) as usize)
                .map(value_i64)
                .unwrap_or(0);
            if pg < 1 || pg > self.mx_pgno as i64 {
                self.pgno = 1;
                self.mx_pgno = 0;
            } else {
                self.pgno = pg as u32;
                self.mx_pgno = self.pgno;
            }
        }
        self.pager = Some(pager);
        Ok(IOResult::Done(self.pgno <= self.mx_pgno))
    }

    fn next(&mut self) -> IOResultOr<bool> {
        self.pgno += 1;
        Ok(IOResult::Done(self.pgno <= self.mx_pgno))
    }

    fn column(&mut self, column: usize) -> IOResultOr<Value> {
        let v = match column {
            0 => Value::from_i64(self.pgno as i64),
            1 => match &self.pager {
                Some(p) => Value::from_slice(&read_page(p, self.pgno)?)?,
                None => Value::Null,
            },
            _ => Value::from_text(self.schema.clone()),
        };
        Ok(IOResult::Done(v))
    }

    fn rowid(&self) -> i64 {
        self.pgno as i64
    }
}

struct DbpageUpdate {
    conn: Arc<Connection>,
    args: Vec<Value>,
}

fn fail(msg: &str) -> LimboError {
    LimboError::SqlError(msg.to_string())
}

impl TableUpdate for DbpageUpdate {
    /// dbpageUpdate: argv is (old rowid, new rowid, pgno, data, schema).
    fn step(&mut self) -> IOResultOr<Option<i64>> {
        Ok(IOResult::Done(self.write()?))
    }
}

impl DbpageUpdate {
    fn write(&mut self) -> Result<Option<i64>> {
        let a = &self.args;
        if self.conn.is_defensive() {
            return Err(fail("read-only"));
        }
        // A delete arrives as (rowid) or, from turso's DELETE, (rowid, NULL).
        if a.len() <= 2 {
            return Err(fail("cannot delete"));
        }
        if a.len() < 5 {
            return Err(fail("bad page value"));
        }
        let is_insert = matches!(a[0], Value::Null);
        let pgno64 = if is_insert {
            // turso may pass the INTEGER PRIMARY KEY as the new rowid.
            let pgno = if matches!(a[2], Value::Null) {
                &a[1]
            } else {
                &a[2]
            };
            value_i64(pgno)
        } else {
            let p = value_i64(&a[0]) as u32 as i64;
            if value_i64(&a[1]) != p {
                return Err(fail("cannot insert"));
            }
            p
        };
        let (db_id, _) = if matches!(a[4], Value::Null) {
            (crate::MAIN_DB_ID, String::new())
        } else {
            find_db(&self.conn, &value_text(&a[4])).ok_or_else(|| fail("no such schema"))?
        };
        if !(1..=4294967294).contains(&pgno64) {
            return Err(fail("bad page number"));
        }
        let pager = self.conn.get_pager_from_database_index(&db_id)?;
        let size = page_size(&pager);
        let mut pgno = pgno64 as u32;
        let mut truncate_to = None;
        let data = match &a[3] {
            Value::Blob(b) if b.len() == size => Some(b.as_slice()),
            Value::Null if is_insert && pgno > 1 => {
                // INSERT (N, NULL) drops page N and everything after it.
                truncate_to = Some(pgno - 1);
                pgno = 1;
                None
            }
            _ => return Err(fail("bad page value")),
        };
        let last = last_page(&pager)?;
        if pgno > last {
            // Writing past the end grows the file, as in SQLite.
            pager
                .io
                .block(|| pager.with_header_mut(|h| h.database_size = pgno.into()))?;
        }
        let (page, c) = pager.io.block(|| pager.read_page(pgno as i64))?;
        if let Some(c) = c {
            pager.io.wait_for_completion(c)?;
        }
        pager.add_dirty(&page)?;
        if let Some(data) = data {
            let contents = page.get_contents();
            let buf = contents
                .buffer()
                .ok_or_else(|| LimboError::InternalError("page not loaded".into()))?;
            buf.as_mut_slice()[..size].copy_from_slice(data);
        }
        if let Some(n) = truncate_to {
            if n < last {
                pager
                    .io
                    .block(|| pager.with_header_mut(|h| h.database_size = n.into()))?;
            }
        }
        Ok(None)
    }
}
