//! The %_content, %_docsize and %_config tables and the averages record
//! (fts5_storage.c).

use super::config::{
    numeric_int, value_text, ConfigValues, Content, Fts5Config, CURRENT_VERSION,
    CURRENT_VERSION_SECUREDELETE,
};
use super::index::{charlen_to_bytelen, decode_sizes, entry_cksum, Index, Pending};
use super::sql::{self, corrupt, err};
use super::tokenize::{TOKENIZE_DOCUMENT, TOKEN_COLOCATED};
use super::varint::put_varint;
use crate::sync::Arc;
use crate::{Connection, Result, Value};
use std::collections::HashSet;

pub(crate) const MAX_TOKEN_SIZE: usize = 32768;
const LOCALE_HDR: [u8; 4] = [0x00, 0xE0, 0xB2, 0xEB];

/// Per-connection write state: pending index data and row/size totals.
#[derive(Debug, Clone)]
pub(crate) struct Writes {
    pub pending: Pending,
    /// (row count, per-column token totals) while they differ from disk.
    pub totals: Option<(i64, Vec<i64>)>,
}

/// An fts5_locale() value: (locale, text).
pub(crate) fn decode_locale(v: &Value) -> Option<(Vec<u8>, Vec<u8>)> {
    let Value::Blob(b) = v else { return None };
    if b.len() <= 4 || b[..4] != LOCALE_HDR {
        return None;
    }
    let rest = &b[4..];
    let nul = rest.iter().position(|&c| c == 0)?;
    Some((rest[..nul].to_vec(), rest[nul + 1..].to_vec()))
}

pub(crate) fn is_locale_value(v: &Value) -> bool {
    matches!(v, Value::Blob(b) if b.len() > 4 && b[..4] == LOCALE_HDR)
}

pub(crate) fn make_locale_value(locale: &[u8], text: &[u8]) -> Vec<u8> {
    let mut out = LOCALE_HDR.to_vec();
    out.extend_from_slice(locale);
    out.push(0);
    out.extend_from_slice(text);
    out
}

/// One column value split into (text, locale).
fn text_and_locale(
    cfg: &Fts5Config,
    v: &Value,
    locale_col: Option<&Value>,
) -> (Vec<u8>, Option<Vec<u8>>) {
    if cfg.locale {
        if let Some((loc, text)) = decode_locale(v) {
            return (text, Some(loc));
        }
    }
    let text = value_text(v).unwrap_or_default();
    let loc = locale_col.and_then(value_text).filter(|l| !l.is_empty());
    (text, loc)
}

/// Shadow-table access for one operation.
pub(crate) struct Storage<'a> {
    pub conn: &'a Arc<Connection>,
    pub cfg: &'a Fts5Config,
    pub index: Index<'a>,
}

impl<'a> Storage<'a> {
    pub(crate) fn new(
        conn: &'a Arc<Connection>,
        cfg: &'a Fts5Config,
        vals: &'a ConfigValues,
    ) -> Self {
        Self {
            conn,
            cfg,
            index: Index::new(conn, cfg, vals),
        }
    }

    /// Creates the shadow tables of a new table, in SQLite's order.
    pub(crate) fn create_tables(conn: &Arc<Connection>, cfg: &Fts5Config) -> Result<()> {
        let create = |suffix: &str, defn: &str, without_rowid: bool| -> Result<()> {
            let name = config_ddl_name(cfg, suffix);
            sql::exec(
                conn,
                &format!(
                    "CREATE TABLE {name}({defn}){}",
                    if without_rowid { " WITHOUT ROWID" } else { "" }
                ),
                &[],
            )
            .map_err(|e| {
                err(format!(
                    "fts5: error creating shadow table {}_{}: {}",
                    cfg.name, suffix, e
                ))
            })
        };
        create("data", "id INTEGER PRIMARY KEY, block BLOB", false)?;
        create("idx", "segid, term, pgno, PRIMARY KEY(segid, term)", true)?;
        let vals = ConfigValues::default();
        let index = Index::new(conn, cfg, &vals);
        index.reinit(0)?;
        if matches!(cfg.content, Content::Normal | Content::Unindexed) {
            let mut defn = String::from("id INTEGER PRIMARY KEY");
            for i in 0..cfg.ncol() {
                if cfg.content == Content::Normal || cfg.unindexed[i] {
                    defn.push_str(&format!(", c{i}"));
                }
            }
            if cfg.locale {
                for i in 0..cfg.ncol() {
                    if !cfg.unindexed[i] {
                        defn.push_str(&format!(", l{i}"));
                    }
                }
            }
            create("content", &defn, false)?;
        }
        if cfg.columnsize {
            let cols = if cfg.contentless_delete {
                "id INTEGER PRIMARY KEY, sz BLOB, origin INTEGER"
            } else {
                "id INTEGER PRIMARY KEY, sz BLOB"
            };
            create("docsize", cols, false)?;
        }
        create("config", "k PRIMARY KEY, v", true)?;
        set_config(conn, cfg, "version", &sql::int(CURRENT_VERSION))
    }

    /// Drops the shadow tables (sqlite3Fts5DropAll).
    pub(crate) fn drop_tables(conn: &Arc<Connection>, cfg: &Fts5Config) -> Result<()> {
        let mut names = vec!["data", "idx", "config"];
        if cfg.columnsize {
            names.push("docsize");
        }
        if cfg.content == Content::Normal {
            names.push("content");
        }
        for n in names {
            sql::exec(
                conn,
                &format!("DROP TABLE IF EXISTS {}", config_ddl_name(cfg, n)),
                &[],
            )?;
        }
        Ok(())
    }

    /// Renames the shadow tables (sqlite3Fts5StorageRename).
    pub(crate) fn rename_tables(conn: &Arc<Connection>, cfg: &Fts5Config, new: &str) -> Result<()> {
        let mut names = vec!["data", "idx", "config"];
        if cfg.columnsize {
            names.push("docsize");
        }
        if cfg.content == Content::Normal {
            names.push("content");
        }
        for n in names {
            sql::exec(
                conn,
                &format!(
                    "ALTER TABLE {} RENAME TO {}",
                    config_ddl_name(cfg, n),
                    super::config::quote(&format!("{new}_{n}"))
                ),
                &[],
            )?;
        }
        Ok(())
    }

    /// Loads %_config into `vals`; errors on an unknown file format.
    pub(crate) fn load_config(
        conn: &Arc<Connection>,
        cfg: &Fts5Config,
        cookie: i32,
    ) -> Result<ConfigValues> {
        let mut vals = ConfigValues::default();
        let rows = sql::query(
            conn,
            &format!("SELECT k, v FROM {}", cfg.shadow("config")),
            &[],
        )?;
        let mut version = 0;
        for row in rows {
            let k = String::from_utf8_lossy(&sql::value_bytes(&row[0])).into_owned();
            if k.eq_ignore_ascii_case("version") {
                version = sql::value_i64(&row[1]);
            } else {
                vals.set(&k, &row[1]);
            }
        }
        if version != CURRENT_VERSION && version != CURRENT_VERSION_SECUREDELETE {
            return Err(err(format!(
                "invalid fts5 file format (found {version}, expected {CURRENT_VERSION} or {CURRENT_VERSION_SECUREDELETE}) - run 'rebuild'"
            )));
        }
        vals.version = version;
        vals.cookie = cookie;
        Ok(vals)
    }

    /// The row count and column totals, from pending state or disk.
    pub(crate) fn totals(&self, w: Option<&Writes>) -> Result<(i64, Vec<i64>)> {
        if let Some((n, s)) = w.and_then(|w| w.totals.as_ref()) {
            return Ok((*n, s.clone()));
        }
        self.index.get_averages()
    }

    fn load_totals<'w>(&self, w: &'w mut Writes) -> Result<&'w mut (i64, Vec<i64>)> {
        if w.totals.is_none() {
            w.totals = Some(self.index.get_averages()?);
        }
        Ok(w.totals.as_mut().unwrap())
    }

    /// The content row (rowid, columns..., locales...) for `rowid`.
    pub(crate) fn lookup(&self, rowid: i64) -> Result<Option<Vec<Value>>> {
        let Some(table) = &self.cfg.content_table else {
            return Ok(None);
        };
        sql::query_row(
            self.conn,
            &format!(
                "SELECT {} FROM {} T WHERE T.{}=?",
                self.cfg.content_exprlist,
                table,
                super::config::quote(&self.cfg.content_rowid)
            ),
            &[sql::int(rowid)],
        )
    }

    /// Tokenizes `text`, calling `f(token, position)`; returns the token count.
    pub(crate) fn tokenize_doc(
        &self,
        flags: i32,
        text: &[u8],
        locale: Option<&[u8]>,
        f: &mut dyn FnMut(&[u8], i32),
    ) -> Result<i32> {
        let mut sz = 0i32;
        let rc = self.cfg.tokenizer().map_err(err)?.tokenize(
            flags,
            text,
            locale,
            &mut |tflags, tok, _, _| {
                let tok = &tok[..tok.len().min(MAX_TOKEN_SIZE)];
                if (tflags & TOKEN_COLOCATED) == 0 || sz == 0 {
                    sz += 1;
                }
                f(tok, sz - 1);
                0
            },
        );
        if rc != 0 {
            return Err(err(format!("tokenizer error {rc}")));
        }
        Ok(sz)
    }

    /// Adds delete markers for a row (fts5StorageDeleteFromIndex). With
    /// `vals` the column values are given; otherwise they are read.
    fn delete_from_index(&self, w: &mut Writes, rowid: i64, vals: Option<&[Value]>) -> Result<()> {
        let row;
        let (cols, locales): (Vec<Value>, Option<Vec<Value>>) = match vals {
            Some(v) => (v.to_vec(), None),
            None => {
                let Some(r) = self.lookup(rowid)? else {
                    return Ok(());
                };
                row = r;
                let n = self.cfg.ncol();
                let cols = row[1..1 + n].to_vec();
                let locs = (self.cfg.locale && row.len() > 1 + n).then(|| row[1 + n..].to_vec());
                (cols, locs)
            }
        };
        self.index.begin_write(&mut w.pending, true, rowid)?;
        let ncol = self.cfg.ncol();
        let mut sizes = vec![0i64; ncol];
        for i in 0..ncol {
            if self.cfg.unindexed[i] {
                continue;
            }
            let (text, loc) = text_and_locale(self.cfg, &cols[i], locales.as_ref().map(|l| &l[i]));
            let pending = &mut w.pending;
            let sz =
                self.tokenize_doc(TOKENIZE_DOCUMENT, &text, loc.as_deref(), &mut |tok, _| {
                    self.index.write_token(pending, -1, 0, tok);
                })?;
            sizes[i] = sz as i64;
        }
        let totals = self.load_totals(w)?;
        for (t, s) in totals.1.iter_mut().zip(&sizes) {
            *t -= s;
            if *t < 0 {
                return Err(corrupt());
            }
        }
        if totals.0 < 1 {
            return Err(corrupt());
        }
        totals.0 -= 1;
        Ok(())
    }

    /// sqlite3Fts5StorageDelete
    pub(crate) fn delete(&self, w: &mut Writes, rowid: i64, vals: Option<&[Value]>) -> Result<()> {
        self.load_totals(w)?;
        if self.cfg.contentless_delete {
            self.index.begin_write(&mut w.pending, true, rowid)?;
            self.contentless_delete(w, rowid)?;
        } else {
            self.delete_from_index(w, rowid, vals)?;
        }
        if self.cfg.columnsize {
            sql::exec(
                self.conn,
                &format!("DELETE FROM {} WHERE id=?", self.cfg.shadow("docsize")),
                &[sql::int(rowid)],
            )?;
        }
        if matches!(self.cfg.content, Content::Normal | Content::Unindexed) {
            sql::exec(
                self.conn,
                &format!("DELETE FROM {} WHERE id=?", self.cfg.shadow("content")),
                &[sql::int(rowid)],
            )?;
        }
        Ok(())
    }

    fn contentless_delete(&self, w: &mut Writes, rowid: i64) -> Result<()> {
        let row = sql::query_row(
            self.conn,
            &format!(
                "SELECT sz, origin FROM {} WHERE id=?",
                self.cfg.shadow("docsize")
            ),
            &[sql::int(rowid)],
        )?;
        let mut origin = 0;
        if let Some(row) = row {
            let blob = sql::value_bytes(&row[0]);
            let sizes = decode_sizes(&blob, self.cfg.ncol()).ok_or_else(corrupt)?;
            let totals = self.load_totals(w)?;
            for (t, s) in totals.1.iter_mut().zip(sizes) {
                *t -= s;
            }
            origin = sql::value_i64(&row[1]);
        }
        if origin != 0 {
            self.index
                .contentless_delete(&mut w.pending, origin, rowid)?;
            w.totals.as_mut().unwrap().0 -= 1;
        }
        Ok(())
    }

    /// sqlite3Fts5StorageContentInsert: writes %_content (or allocates a
    /// rowid) and returns the row's rowid. `saved` is the old row for an
    /// UPDATE of unchanged columns.
    pub(crate) fn content_insert(
        &self,
        replace: bool,
        vals: &[Value],
        saved: Option<&[Value]>,
    ) -> Result<i64> {
        let cfg = self.cfg;
        if !matches!(cfg.content, Content::Normal | Content::Unindexed) {
            if let Value::Numeric(crate::numeric::Numeric::Integer(r)) = vals[1] {
                return Ok(r);
            }
            return self.new_rowid();
        }
        let ncol = cfg.ncol();
        let mut cols = vec!["id".to_string()];
        let mut params = vec![vals[1].clone()];
        for i in 0..ncol {
            if cfg.content == Content::Normal || cfg.unindexed[i] {
                let mut v = vals[2 + i].clone();
                let _ = saved;
                if let Some((loc, text)) = decode_locale(&v) {
                    v = Value::build_text(String::from_utf8_lossy(&text).into_owned());
                    if !cfg.unindexed[i] {
                        cols.push(format!("l{i}"));
                        params.push(Value::build_text(
                            String::from_utf8_lossy(&loc).into_owned(),
                        ));
                    }
                }
                cols.push(format!("c{i}"));
                params.push(v);
            }
        }
        let placeholders = vec!["?"; params.len()].join(",");
        sql::exec(
            self.conn,
            &format!(
                "{} INTO {}({}) VALUES({})",
                if replace { "REPLACE" } else { "INSERT" },
                cfg.shadow("content"),
                cols.join(","),
                placeholders
            ),
            &params,
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Allocates a rowid through %_docsize (fts5StorageNewRowid).
    fn new_rowid(&self) -> Result<i64> {
        if !self.cfg.columnsize {
            return Err(crate::LimboError::SqlError("datatype mismatch".into()));
        }
        sql::exec(
            self.conn,
            &format!(
                "REPLACE INTO {} VALUES(?,?{})",
                self.cfg.shadow("docsize"),
                if self.cfg.contentless_delete {
                    ",?"
                } else {
                    ""
                }
            ),
            &if self.cfg.contentless_delete {
                vec![Value::Null, Value::Null, Value::Null]
            } else {
                vec![Value::Null, Value::Null]
            },
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// sqlite3Fts5StorageIndexInsert
    pub(crate) fn index_insert(&self, w: &mut Writes, vals: &[Value], rowid: i64) -> Result<()> {
        self.load_totals(w)?;
        self.index.begin_write(&mut w.pending, false, rowid)?;
        let ncol = self.cfg.ncol();
        let mut sizes = vec![0i64; ncol];
        let mut buf = Vec::new();
        for i in 0..ncol {
            let mut sz = 0;
            if !self.cfg.unindexed[i] {
                let (text, loc) = text_and_locale(self.cfg, &vals[2 + i], None);
                let pending = &mut w.pending;
                sz = self.tokenize_doc(
                    TOKENIZE_DOCUMENT,
                    &text,
                    loc.as_deref(),
                    &mut |tok, pos| {
                        self.index.write_token(pending, i as i32, pos, tok);
                    },
                )?;
            }
            put_varint(&mut buf, sz as u64);
            sizes[i] = sz as i64;
        }
        let totals = self.load_totals(w)?;
        for (t, s) in totals.1.iter_mut().zip(&sizes) {
            *t += s;
        }
        totals.0 += 1;
        self.insert_docsize(w, rowid, &buf)
    }

    fn insert_docsize(&self, w: &Writes, rowid: i64, sz: &[u8]) -> Result<()> {
        if !self.cfg.columnsize {
            return Ok(());
        }
        if self.cfg.contentless_delete {
            let _ = w;
            let origin = self.index.origin()? as i64;
            sql::exec(
                self.conn,
                &format!("REPLACE INTO {} VALUES(?,?,?)", self.cfg.shadow("docsize")),
                &[sql::int(rowid), sql::blob(sz), sql::int(origin)],
            )
        } else {
            sql::exec(
                self.conn,
                &format!("REPLACE INTO {} VALUES(?,?)", self.cfg.shadow("docsize")),
                &[sql::int(rowid), sql::blob(sz)],
            )
        }
    }

    /// Per-column token counts of a row from %_docsize.
    pub(crate) fn docsize(&self, rowid: i64) -> Result<Vec<i64>> {
        let row = sql::query_row(
            self.conn,
            &format!("SELECT sz FROM {} WHERE id=?", self.cfg.shadow("docsize")),
            &[sql::int(rowid)],
        )?;
        let Some(row) = row else {
            return Err(corrupt());
        };
        decode_sizes(&sql::value_bytes(&row[0]), self.cfg.ncol()).ok_or_else(corrupt)
    }

    /// Writes pending totals and index data (sqlite3Fts5StorageSync).
    pub(crate) fn sync(&self, w: &mut Writes) -> Result<()> {
        let last = self.conn.last_insert_rowid();
        if let Some((n, sizes)) = w.totals.take() {
            self.index.set_averages(n, &sizes)?;
        }
        self.index.flush(&mut w.pending)?;
        w.pending.started = false;
        self.conn.update_last_rowid(last);
        Ok(())
    }

    /// sqlite3Fts5StorageDeleteAll
    pub(crate) fn delete_all(&self, w: &mut Writes) -> Result<()> {
        w.totals = None;
        w.pending.discard();
        sql::exec(
            self.conn,
            &format!("DELETE FROM {}", self.cfg.shadow("data")),
            &[],
        )?;
        sql::exec(
            self.conn,
            &format!("DELETE FROM {}", self.cfg.shadow("idx")),
            &[],
        )?;
        if self.cfg.columnsize {
            sql::exec(
                self.conn,
                &format!("DELETE FROM {}", self.cfg.shadow("docsize")),
                &[],
            )?;
        }
        if self.cfg.content == Content::Unindexed {
            sql::exec(
                self.conn,
                &format!("DELETE FROM {}", self.cfg.shadow("content")),
                &[],
            )?;
        }
        self.index.reinit(self.index.vals.cookie.max(0))?;
        set_config(self.conn, self.cfg, "version", &sql::int(CURRENT_VERSION))
    }

    /// sqlite3Fts5StorageRebuild
    pub(crate) fn rebuild(&self, w: &mut Writes) -> Result<()> {
        self.delete_all(w)?;
        w.totals = Some((0, vec![0; self.cfg.ncol()]));
        let Some(table) = &self.cfg.content_table else {
            return Ok(());
        };
        let rows = sql::query(
            self.conn,
            &format!("SELECT {} FROM {} AS T", self.cfg.content_exprlist, table),
            &[],
        )?;
        let ncol = self.cfg.ncol();
        for row in rows {
            let rowid = sql::value_i64(&row[0]);
            self.index.begin_write(&mut w.pending, false, rowid)?;
            let mut buf = Vec::new();
            for i in 0..ncol {
                let mut sz = 0;
                if !self.cfg.unindexed[i] {
                    let loc_col =
                        (self.cfg.locale && row.len() > 1 + ncol).then(|| &row[1 + ncol + i]);
                    let (text, loc) = text_and_locale(self.cfg, &row[1 + i], loc_col);
                    let pending = &mut w.pending;
                    sz = self.tokenize_doc(
                        TOKENIZE_DOCUMENT,
                        &text,
                        loc.as_deref(),
                        &mut |tok, pos| {
                            self.index.write_token(pending, i as i32, pos, tok);
                        },
                    )?;
                }
                put_varint(&mut buf, sz as u64);
                let t = w.totals.as_mut().unwrap();
                t.1[i] += sz as i64;
            }
            w.totals.as_mut().unwrap().0 += 1;
            self.insert_docsize(w, rowid, &buf)?;
        }
        Ok(())
    }

    /// Writes a %_config value and bumps the cookie (sqlite3Fts5StorageConfigValue).
    pub(crate) fn config_value(&self, key: &str, val: &Value) -> Result<i32> {
        set_config(self.conn, self.cfg, key, val)?;
        let new = self.index.vals.cookie + 1;
        self.index.set_cookie(new)?;
        Ok(new)
    }

    /// sqlite3Fts5StorageIntegrity
    pub(crate) fn integrity(&self, w: Option<&Writes>, arg: i64) -> Result<()> {
        let cfg = self.cfg;
        let ncol = cfg.ncol();
        let use_cksum =
            cfg.content == Content::Normal || (cfg.content == Content::External && arg != 0);
        let mut cksum = 0u64;
        if use_cksum {
            let table = cfg.content_table.as_ref().expect("content table");
            let rows = sql::query(
                self.conn,
                &format!("SELECT {} FROM {} AS T", cfg.content_exprlist, table),
                &[],
            )?;
            let mut total = vec![0i64; ncol];
            for row in &rows {
                let rowid = sql::value_i64(&row[0]);
                let colsize = if cfg.columnsize {
                    Some(self.docsize(rowid)?)
                } else {
                    None
                };
                let mut termset: HashSet<(usize, Vec<u8>)> = HashSet::new();
                for i in 0..ncol {
                    if cfg.unindexed[i] {
                        continue;
                    }
                    let loc_col =
                        (cfg.content == Content::Normal && cfg.locale && row.len() > 1 + ncol)
                            .then(|| &row[1 + ncol + i]);
                    let (text, loc) = text_and_locale(cfg, &row[1 + i], loc_col);
                    if cfg.detail == super::config::Detail::Columns {
                        termset.clear();
                    }
                    let detail = cfg.detail;
                    let sz = self.tokenize_doc(
                        TOKENIZE_DOCUMENT,
                        &text,
                        loc.as_deref(),
                        &mut |tok, pos| {
                            let (ipos, icol) = match detail {
                                super::config::Detail::Full => (pos, i as i32),
                                super::config::Detail::Columns => (i as i32, 0),
                                super::config::Detail::None => (0, 0),
                            };
                            let fresh = detail == super::config::Detail::Full
                                || termset.insert((0, tok.to_vec()));
                            if fresh {
                                cksum ^= entry_cksum(rowid, icol, ipos, 0, tok);
                            }
                            for (ii, &nchar) in cfg.prefixes.iter().enumerate() {
                                let nbyte = charlen_to_bytelen(tok, nchar);
                                if nbyte > 0 {
                                    let fresh = detail == super::config::Detail::Full
                                        || termset.insert((ii + 1, tok[..nbyte].to_vec()));
                                    if fresh {
                                        cksum ^= entry_cksum(
                                            rowid,
                                            icol,
                                            ipos,
                                            ii as i32 + 1,
                                            &tok[..nbyte],
                                        );
                                    }
                                }
                            }
                        },
                    )?;
                    if let Some(cs) = &colsize {
                        if sz as i64 != cs[i] {
                            return Err(corrupt());
                        }
                    }
                    total[i] += sz as i64;
                }
            }
            let (nrow, sizes) = self.totals(w)?;
            if sizes != total {
                return Err(corrupt());
            }
            if cfg.content == Content::Normal {
                let n = count(self.conn, &cfg.shadow("content"))?;
                if n != nrow {
                    return Err(corrupt());
                }
            }
            if cfg.columnsize {
                let n = count(self.conn, &cfg.shadow("docsize"))?;
                if n != nrow {
                    return Err(corrupt());
                }
            }
        }
        self.index
            .integrity_check(w.map(|w| &w.pending), cksum, use_cksum)
    }
}

/// `REPLACE INTO %_config VALUES(key, val)`, as a delete and an insert
/// because the table is WITHOUT ROWID.
pub(crate) fn set_config(
    conn: &Arc<Connection>,
    cfg: &Fts5Config,
    key: &str,
    val: &Value,
) -> Result<()> {
    let table = cfg.shadow("config");
    sql::exec(
        conn,
        &format!("DELETE FROM {table} WHERE k=?"),
        &[sql::text(key)],
    )?;
    sql::exec(
        conn,
        &format!("INSERT INTO {table} VALUES(?,?)"),
        &[sql::text(key), val.clone()],
    )
}

fn count(conn: &Arc<Connection>, table: &str) -> Result<i64> {
    let row = sql::query_row(conn, &format!("SELECT count(*) FROM {table}"), &[])?;
    Ok(row.map_or(0, |r| sql::value_i64(&r[0])))
}

/// The name to use in DDL: unqualified for "main", as SQLite stores it.
fn config_ddl_name(cfg: &Fts5Config, suffix: &str) -> String {
    let name = super::config::quote(&format!("{}_{}", cfg.name, suffix));
    if cfg.db.eq_ignore_ascii_case("main") {
        name
    } else {
        format!("{}.{}", super::config::quote(&cfg.db), name)
    }
}

#[allow(dead_code)]
fn _unused(v: &Value) -> Option<i64> {
    numeric_int(v)
}
