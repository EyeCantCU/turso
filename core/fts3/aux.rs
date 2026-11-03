//! The fts4aux virtual table (`fts3_aux.c`): one row per term and column.

use super::segment::{
    seg_reader_cursor, MultiSegReader, SegFilter, CURSOR_ALL, IGNORE_EMPTY, REQUIRE_POS, SCAN,
};
use super::table::{Ctx, Fts3Table};
use super::{corrupt, dequote, get_varint, sql_error, value_int, value_text};
use crate::ext::{ModuleImplementation, VTabImpl};
use crate::native_ext::{
    ConnectInfo, ConstraintInfo, ConstraintUsage, IndexInfo, OrderByInfo, VTabKind, VirtualTable,
    VirtualTableCursor, VirtualTableModule,
};
use crate::sync::Arc;
use crate::types::IOResultOr;
use crate::{Connection, IOResult, Result, SymbolTable, Value};
use turso_ext::{ConstraintOp, ResultCode};

const SCHEMA: &str = "CREATE TABLE x(term, col, documents, occurrences, languageid HIDDEN)";
const EQ: i32 = 1;
const GE: i32 = 2;
const LE: i32 = 4;

#[derive(Debug)]
struct AuxModule;

impl VirtualTableModule for AuxModule {
    type Table = AuxTable;

    fn schema(&self, _args: &[Value]) -> Result<String> {
        Ok(SCHEMA.to_string())
    }

    fn create(&self, _args: &[Value]) -> Result<AuxTable> {
        Err(sql_error("invalid arguments to fts4aux constructor"))
    }

    fn connect(&self, info: &ConnectInfo, args: &[Value]) -> Result<(AuxTable, String)> {
        // SQLite's argv is (module, db, table, args...).
        let args: Vec<String> = args
            .iter()
            .map(|v| {
                value_text(v)
                    .map(|t| String::from_utf8_lossy(&t).into_owned())
                    .unwrap_or_default()
            })
            .collect();
        let (db, fts) = match args.len() {
            1 => (info.schema_name.to_string(), args[0].clone()),
            2 if info.schema_name.eq_ignore_ascii_case("temp") => {
                (args[0].clone(), args[1].clone())
            }
            _ => return Err(sql_error("invalid arguments to fts4aux constructor")),
        };
        let tab = Fts3Table::aux(&db, &dequote(&fts));
        Ok((AuxTable { tab: Arc::new(tab) }, SCHEMA.to_string()))
    }

    fn innocuous(&self) -> bool {
        true
    }
}

#[derive(Debug)]
struct AuxTable {
    tab: Arc<Fts3Table>,
}

impl VirtualTable for AuxTable {
    type Cursor = AuxCursor;

    fn open(&self, conn: Arc<Connection>) -> Result<AuxCursor> {
        Ok(AuxCursor {
            tab: self.tab.clone(),
            conn,
            csr: MultiSegReader::default(),
            stop: None,
            langid: 0,
            eof: true,
            rowid: 0,
            icol: 0,
            stat: Vec::new(),
        })
    }

    fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> std::result::Result<IndexInfo, ResultCode> {
        let (mut ieq, mut ige, mut ile, mut ilang) = (None, None, None, None);
        for (i, c) in constraints.iter().enumerate() {
            if !c.usable {
                continue;
            }
            if c.column_index == 0 {
                match c.op {
                    ConstraintOp::Eq => ieq = Some(i),
                    ConstraintOp::Lt | ConstraintOp::Le => ile = Some(i),
                    ConstraintOp::Gt | ConstraintOp::Ge => ige = Some(i),
                    _ => {}
                }
            }
            if c.column_index == 4 && c.op == ConstraintOp::Eq {
                ilang = Some(i);
            }
        }
        let mut usages = vec![
            ConstraintUsage {
                argv_index: None,
                omit: false
            };
            constraints.len()
        ];
        let mut next = 1;
        let mut idx_num = 0;
        let mut cost: f64;
        if let Some(i) = ieq {
            idx_num = EQ;
            usages[i].argv_index = Some(next);
            next += 1;
            cost = 5.0;
        } else {
            cost = 20000.0;
            if let Some(i) = ige {
                idx_num += GE;
                usages[i].argv_index = Some(next);
                next += 1;
                cost /= 2.0;
            }
            if let Some(i) = ile {
                idx_num += LE;
                usages[i].argv_index = Some(next);
                next += 1;
                cost /= 2.0;
            }
        }
        if let Some(i) = ilang {
            usages[i].argv_index = Some(next);
            cost -= 1.0;
        }
        let consumed = order_by.len() == 1 && order_by[0].column_index == 0 && !order_by[0].desc;
        Ok(IndexInfo {
            idx_num,
            idx_str: None,
            order_by_consumed: consumed,
            estimated_cost: cost,
            estimated_rows: 25,
            constraint_usages: usages,
        })
    }
}

struct AuxCursor {
    tab: Arc<Fts3Table>,
    conn: Arc<Connection>,
    csr: MultiSegReader,
    stop: Option<Vec<u8>>,
    langid: i32,
    eof: bool,
    rowid: i64,
    icol: usize,
    /// (documents, occurrences); index 0 is "*", column i is i+1.
    stat: Vec<(i64, i64)>,
}

impl AuxCursor {
    fn advance(&mut self) -> Result<()> {
        self.rowid += 1;
        self.icol += 1;
        while self.icol < self.stat.len() {
            if self.stat[self.icol].0 > 0 {
                return Ok(());
            }
            self.icol += 1;
        }
        let ctx = Ctx {
            conn: &self.conn,
            tab: &self.tab,
        };
        if !self.csr.step(&ctx)? {
            self.eof = true;
            return Ok(());
        }
        if let Some(stop) = &self.stop {
            let term = &self.csr.term;
            let n = stop.len().min(term.len());
            let mc = stop[..n].cmp(&term[..n]);
            if mc == std::cmp::Ordering::Less
                || (mc == std::cmp::Ordering::Equal && term.len() > stop.len())
            {
                self.eof = true;
                return Ok(());
            }
        }
        if self.stat.len() < 2 {
            self.stat.resize(2, (0, 0));
        }
        self.stat.iter_mut().for_each(|s| *s = (0, 0));
        let d = &self.csr.doclist;
        let mut i = 0;
        let mut state = 0;
        let mut icol = 0usize;
        while i < d.len() {
            let (v, n) = get_varint(d, i);
            i += n;
            match state {
                0 => {
                    self.stat[0].0 += 1;
                    state = 1;
                    icol = 0;
                }
                3 => {
                    if !(1..=32767).contains(&v) {
                        return Err(corrupt());
                    }
                    icol = v as usize;
                    if self.stat.len() < icol + 2 {
                        self.stat.resize(icol + 2, (0, 0));
                    }
                    self.stat[icol + 1].0 += 1;
                    state = 2;
                }
                _ => {
                    if state == 1 {
                        if v > 1 {
                            self.stat[1].0 += 1;
                        }
                        state = 2;
                    }
                    if v == 0 {
                        state = 0;
                    } else if v == 1 {
                        state = 3;
                    } else {
                        self.stat[icol + 1].1 += 1;
                        self.stat[0].1 += 1;
                    }
                }
            }
        }
        self.icol = 0;
        Ok(())
    }
}

impl VirtualTableCursor for AuxCursor {
    fn filter(&mut self, args: &[Value], _idx_str: Option<&str>, idx_num: i32) -> IOResultOr<bool> {
        let mut next = 0;
        let (mut ieq, mut ige, mut ile) = (None, None, None);
        let mut scan = false;
        if idx_num == EQ {
            ieq = Some(next);
            next += 1;
        } else {
            scan = true;
            if idx_num & GE != 0 {
                ige = Some(next);
                next += 1;
            }
            if idx_num & LE != 0 {
                ile = Some(next);
                next += 1;
            }
        }
        let ilang = if next < args.len() { Some(next) } else { None };
        self.csr = MultiSegReader::default();
        self.stat.clear();
        self.stop = None;
        self.rowid = 0;
        self.icol = 0;
        self.eof = false;
        let first_is_text = matches!(args.first(), Some(Value::Text(_)));
        let mut term = None;
        if (ieq.is_some() || ige.is_some()) && first_is_text {
            term = value_text(&args[0]);
        }
        if let Some(i) = ile {
            if first_is_text {
                self.stop = value_text(&args[i]);
            }
        }
        self.langid = ilang
            .map(|i| value_int(&args[i]).max(0) as i32)
            .unwrap_or(0);
        let ctx = Ctx {
            conn: &self.conn,
            tab: &self.tab,
        };
        let mut flags = REQUIRE_POS | IGNORE_EMPTY;
        if scan {
            flags |= SCAN;
        }
        seg_reader_cursor(
            &ctx,
            self.langid,
            0,
            CURSOR_ALL,
            term.as_deref(),
            false,
            scan,
            &mut self.csr,
        )?;
        self.csr.start(
            &ctx,
            SegFilter {
                term,
                icol: 0,
                flags,
            },
        )?;
        self.advance()?;
        Ok(IOResult::Done(!self.eof))
    }

    fn next(&mut self) -> IOResultOr<bool> {
        self.advance()?;
        Ok(IOResult::Done(!self.eof))
    }

    fn column(&mut self, column: usize) -> IOResultOr<Value> {
        let v = match column {
            0 => Value::build_text(String::from_utf8_lossy(&self.csr.term).into_owned()),
            1 => {
                if self.icol > 0 {
                    Value::from_i64(self.icol as i64 - 1)
                } else {
                    Value::build_text("*")
                }
            }
            2 => Value::from_i64(self.stat.get(self.icol).map(|s| s.0).unwrap_or(0)),
            3 => Value::from_i64(self.stat.get(self.icol).map(|s| s.1).unwrap_or(0)),
            _ => Value::from_i64(self.langid as i64),
        };
        Ok(IOResult::Done(v))
    }

    fn rowid(&self) -> i64 {
        self.rowid
    }
}

pub(crate) fn register(syms: &mut SymbolTable) {
    let module = VTabImpl {
        module_kind: VTabKind::VirtualTable,
        implementation: ModuleImplementation::Native(Arc::new(AuxModule)),
        eponymous: None,
    };
    syms.vtab_modules
        .insert("fts4aux".to_string(), Arc::new(module));
}
