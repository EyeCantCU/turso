//! The auxiliary-function API (Fts5ExtensionApi) and the built-in
//! functions bm25(), highlight() and snippet() (fts5_aux.c).

use super::sql::err;
use super::tokenize::TOKEN_COLOCATED;
use crate::{LimboError, Result, Value};
use std::any::Any;

/// SQLITE_RANGE from an API call.
pub const RANGE: i32 = 25;

/// Errors an API method can report: an SQLite result code or an error.
#[derive(Debug)]
pub enum ApiError {
    Code(i32),
    Err(LimboError),
}

impl From<LimboError> for ApiError {
    fn from(e: LimboError) -> Self {
        ApiError::Err(e)
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

/// What an auxiliary function can ask about the current row.
pub trait AuxApi {
    fn column_count(&self) -> i32;
    fn row_count(&mut self) -> ApiResult<i64>;
    fn column_total_size(&mut self, col: i32) -> ApiResult<i64>;
    /// Tokenizes with FTS5_TOKENIZE_AUX. `cb(tflags, token, start, end)`.
    fn tokenize(
        &mut self,
        text: &[u8],
        locale: Option<&[u8]>,
        cb: &mut dyn FnMut(i32, &[u8], i32, i32) -> i32,
    ) -> ApiResult<()>;
    fn phrase_count(&self) -> i32;
    fn phrase_size(&self, phrase: i32) -> i32;
    fn inst_count(&mut self) -> ApiResult<i32>;
    fn inst(&mut self, i: i32) -> ApiResult<(i32, i32, i32)>;
    fn rowid(&self) -> i64;
    /// The text of a column; None for a contentless table.
    fn column_text(&mut self, col: i32) -> ApiResult<Option<Vec<u8>>>;
    fn column_size(&mut self, col: i32) -> ApiResult<i32>;
    /// Calls `cb` once for each row matching phrase `phrase` alone.
    fn query_phrase_count(&mut self, phrase: i32) -> ApiResult<i64>;
    fn column_locale(&mut self, col: i32) -> ApiResult<Option<Vec<u8>>>;
    fn auxdata(&mut self) -> Option<&mut Box<dyn Any + Send>>;
    fn set_auxdata(&mut self, data: Box<dyn Any + Send>);
    /// Removes and returns the auxdata (xGetAuxdata with bClear).
    fn take_auxdata(&mut self) -> Option<Box<dyn Any + Send>>;
    /// The position list of a phrase in the current row (as xPhraseFirst reads it).
    fn phrase_poslist(&mut self, phrase: i32) -> ApiResult<Vec<u8>>;
    /// The column list of a phrase (detail=columns), for xPhraseFirstColumn.
    fn phrase_collist(&mut self, phrase: i32) -> ApiResult<Vec<u8>>;
    /// Calls `cb` for each row matching phrase `phrase` alone; `cb` returns
    /// false to stop (xQueryPhrase).
    fn query_phrase(
        &mut self,
        phrase: i32,
        cb: &mut dyn FnMut(&mut dyn AuxApi) -> ApiResult<bool>,
    ) -> ApiResult<()>;
    /// The text of token `token` of phrase `phrase` in the query.
    fn query_token(&mut self, phrase: i32, token: i32) -> ApiResult<Vec<u8>>;
    /// The detail mode: 0 full, 1 none, 2 columns.
    fn detail(&self) -> i32;
}

fn to_result(e: ApiError) -> LimboError {
    match e {
        ApiError::Err(e) => e,
        ApiError::Code(RANGE) => err("column index out of range"),
        ApiError::Code(11) | ApiError::Code(267) => super::sql::corrupt(),
        ApiError::Code(c) => err(format!("SQL logic error ({c})")),
    }
}

/// The text of a value as `sqlite3_value_text`, NULL as "".
fn text_or_empty(v: Option<&Value>) -> Vec<u8> {
    v.and_then(super::config::value_text).unwrap_or_default()
}

fn clamped_int(v: &Value) -> i32 {
    super::sql::value_i64(v).clamp(-1, 0x7FFF_FFFF) as i32
}

fn value_double(v: &Value) -> f64 {
    match v {
        Value::Numeric(crate::numeric::Numeric::Integer(i)) => *i as f64,
        Value::Numeric(crate::numeric::Numeric::Float(f)) => f64::from(*f),
        Value::Text(t) => t.as_str().trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Runs a built-in auxiliary function. `None` if `name` is not built in.
pub(crate) fn call_builtin(
    name: &str,
    api: &mut dyn AuxApi,
    args: &[Value],
) -> Option<Result<Value>> {
    let r = match name.to_ascii_lowercase().as_str() {
        "bm25" => bm25(api, args),
        "highlight" => highlight(api, args),
        "snippet" => snippet(api, args),
        "fts5_get_locale" => get_locale(api, args),
        _ => return None,
    };
    Some(r.map_err(to_result))
}

pub(crate) fn is_builtin(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "bm25" | "highlight" | "snippet" | "fts5_get_locale"
    )
}

/// Iterates coalesced phrase instances in one column (CInstIter).
struct CInstIter {
    col: i32,
    iinst: i32,
    ninst: i32,
    start: i32,
    end: i32,
}

impl CInstIter {
    fn new(api: &mut dyn AuxApi, col: i32) -> ApiResult<Self> {
        let mut it = CInstIter {
            col,
            iinst: 0,
            ninst: api.inst_count()?,
            start: -1,
            end: -1,
        };
        it.next(api)?;
        Ok(it)
    }

    fn next(&mut self, api: &mut dyn AuxApi) -> ApiResult<()> {
        self.start = -1;
        self.end = -1;
        while self.iinst < self.ninst {
            let (ip, ic, io) = api.inst(self.iinst)?;
            if ic == self.col {
                let end = io - 1 + api.phrase_size(ip);
                if self.start < 0 {
                    self.start = io;
                    self.end = end;
                } else if io <= self.end {
                    if end > self.end {
                        self.end = end;
                    }
                } else {
                    break;
                }
            }
            self.iinst += 1;
        }
        Ok(())
    }
}

struct Highlight<'a> {
    range_start: i32,
    range_end: i32,
    open: &'a [u8],
    close: &'a [u8],
    input: &'a [u8],
    iter: CInstIter,
    pos: i32,
    off: usize,
    is_open: bool,
    out: Vec<u8>,
}

impl Highlight<'_> {
    fn append(&mut self, from: usize, to: usize) {
        let to = to.min(self.input.len());
        if from < to {
            self.out.extend_from_slice(&self.input[from..to]);
        }
    }

    /// fts5HighlightCb
    fn token(&mut self, api: &mut dyn AuxApi, tflags: i32, start: i32, end: i32) -> ApiResult<()> {
        if tflags & TOKEN_COLOCATED != 0 {
            return Ok(());
        }
        let (start, end) = (start.max(0) as usize, end.max(0) as usize);
        let ipos = self.pos;
        self.pos += 1;
        if self.range_end >= 0 {
            if ipos < self.range_start || ipos > self.range_end {
                return Ok(());
            }
            if self.range_start != 0 && ipos == self.range_start {
                self.off = start;
            }
        }
        if self.is_open && (ipos <= self.iter.start || self.iter.start < 0) && start > self.off {
            self.out.extend_from_slice(self.close);
            self.is_open = false;
        }
        if ipos == self.iter.start && !self.is_open {
            self.append(self.off, start);
            self.out.extend_from_slice(self.open);
            self.off = start;
            self.is_open = true;
        }
        if ipos == self.iter.end {
            if !self.is_open {
                self.out.extend_from_slice(self.open);
                self.is_open = true;
            }
            self.append(self.off, end);
            self.off = end;
            self.iter.next(api)?;
        }
        if ipos == self.range_end {
            if self.is_open {
                if self.iter.start >= 0 && ipos >= self.iter.start {
                    self.append(self.off, end);
                    self.off = end;
                }
                self.out.extend_from_slice(self.close);
                self.is_open = false;
            }
            self.append(self.off, end);
            self.off = end;
        }
        Ok(())
    }
}

/// Tokenizes `text`, feeding each token to the highlighter.
fn run_highlight(
    api: &mut dyn AuxApi,
    h: &mut Highlight,
    text: &[u8],
    locale: Option<&[u8]>,
) -> ApiResult<()> {
    let mut toks: Vec<(i32, i32, i32)> = Vec::new();
    api.tokenize(text, locale, &mut |tflags, _tok, s, e| {
        toks.push((tflags, s, e));
        0
    })?;
    for (tflags, s, e) in toks {
        h.token(api, tflags, s, e)?;
    }
    Ok(())
}

fn highlight(api: &mut dyn AuxApi, args: &[Value]) -> ApiResult<Value> {
    if args.len() != 3 {
        return Err(ApiError::Err(err(
            "wrong number of arguments to function highlight()",
        )));
    }
    let col = clamped_int(&args[0]);
    let open = text_or_empty(args.get(1));
    let close = text_or_empty(args.get(2));
    let input = match api.column_text(col) {
        Err(ApiError::Code(RANGE)) => return Ok(Value::build_text("")),
        Err(e) => return Err(e),
        Ok(None) => return Ok(Value::Null),
        Ok(Some(t)) => t,
    };
    let iter = CInstIter::new(api, col)?;
    let locale = api.column_locale(col)?;
    let mut h = Highlight {
        range_start: 0,
        range_end: -1,
        open: &open,
        close: &close,
        input: &input,
        iter,
        pos: 0,
        off: 0,
        is_open: false,
        out: Vec::new(),
    };
    run_highlight(api, &mut h, &input, locale.as_deref())?;
    if h.is_open {
        h.out.extend_from_slice(&close);
    }
    let off = h.off;
    h.append(off, input.len());
    Ok(Value::build_text(
        String::from_utf8_lossy(&h.out).into_owned(),
    ))
}

fn snippet_score(
    api: &mut dyn AuxApi,
    ndocsize: i32,
    seen: &mut [bool],
    col: i32,
    ipos: i32,
    ntoken: i32,
) -> ApiResult<(i32, i32)> {
    let mut score = 0;
    let mut first = -1i64;
    let mut last = 0i64;
    let end = ipos as i64 + ntoken as i64;
    let ninst = api.inst_count()?;
    for i in 0..ninst {
        let (ip, ic, io) = api.inst(i)?;
        if ic == col && io >= ipos && (io as i64) < end {
            score += if seen[ip as usize] { 1 } else { 1000 };
            seen[ip as usize] = true;
            if first < 0 {
                first = io as i64;
            }
            last = io as i64 + api.phrase_size(ip) as i64;
        }
    }
    let mut adj = first - (ntoken as i64 - (last - first)) / 2;
    if adj + ntoken as i64 > ndocsize as i64 {
        adj = ndocsize as i64 - ntoken as i64;
    }
    if adj < 0 {
        adj = 0;
    }
    Ok((score, adj as i32))
}

/// Scores the instances of column `i` for snippet(). `consider(score,
/// start, colsize)` records a candidate and returns the best score so far.
/// An error with `true` means a call failed and the column loop must stop.
fn snippet_column(
    api: &mut dyn AuxApi,
    i: i32,
    ntoken: i32,
    ninst: i32,
    seen: &mut [bool],
    consider: &mut dyn FnMut(i32, i32, i32) -> i32,
) -> std::result::Result<(), (bool, ApiError)> {
    let stop = |e: ApiError| (true, e);
    let cont = |e: ApiError| (false, e);
    let doc = api.column_text(i).map_err(stop)?.unwrap_or_default();
    let locale = api.column_locale(i).map_err(stop)?;
    // Sentence starts: tokens after '.' or ':' and whitespace.
    let mut first: Vec<i32> = Vec::new();
    let mut spos = 0;
    api.tokenize(&doc, locale.as_deref(), &mut |tflags, _t, s, _e| {
        if tflags & TOKEN_COLOCATED == 0 {
            if spos > 0 {
                let mut k = s as i64 - 1;
                let mut c = 0u8;
                while k >= 0 {
                    c = doc[k as usize];
                    if !matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    k -= 1;
                }
                if k != s as i64 - 1 && (c == b'.' || c == b':') {
                    first.push(spos);
                }
            } else {
                first.push(0);
            }
            spos += 1;
        }
        0
    })
    .map_err(stop)?;
    let ndocsize = api.column_size(i).map_err(stop)?;
    for ii in 0..ninst {
        let (_ip, ic, io) = api.inst(ii).map_err(cont)?;
        if ic != i {
            continue;
        }
        if io > ndocsize {
            return Err(cont(ApiError::Err(super::sql::corrupt())));
        }
        seen.iter_mut().for_each(|s| *s = false);
        let (score, adj) = snippet_score(api, ndocsize, seen, i, io, ntoken).map_err(cont)?;
        let best = consider(score, adj, ndocsize);
        if !first.is_empty() && ndocsize > ntoken {
            let mut jj = 0;
            while jj < first.len() - 1 {
                if first[jj + 1] > io {
                    break;
                }
                jj += 1;
            }
            if first[jj] < io {
                seen.iter_mut().for_each(|s| *s = false);
                let (mut score, _) =
                    snippet_score(api, ndocsize, seen, i, first[jj], ntoken).map_err(cont)?;
                score += if first[jj] == 0 { 120 } else { 100 };
                if score > best {
                    consider(score, first[jj], ndocsize);
                }
            }
        }
    }
    Ok(())
}

fn snippet(api: &mut dyn AuxApi, args: &[Value]) -> ApiResult<Value> {
    if args.len() != 5 {
        return Err(ApiError::Err(err(
            "wrong number of arguments to function snippet()",
        )));
    }
    let ncol = api.column_count();
    let col = clamped_int(&args[0]);
    let open = text_or_empty(args.get(1));
    let close = text_or_empty(args.get(2));
    let ellips = text_or_empty(args.get(3));
    let ntoken = super::sql::value_i64(&args[4]).clamp(0, 64) as i32;
    let mut best_col = if col >= 0 { col } else { 0 };
    let nphrase = api.phrase_count().max(0) as usize;
    let mut seen = vec![false; nphrase];
    let ninst = api.inst_count()?;
    let mut best_start = 0;
    let mut best_score = 0;
    let mut ncolsize = 0;
    // Errors follow fts5SnippetFunction's rc: a later column's calls
    // overwrite an error from an earlier column.
    let mut rc: ApiResult<()> = Ok(());
    for i in 0..ncol {
        if !(col < 0 || col == i) {
            continue;
        }
        let r = snippet_column(
            api,
            i,
            ntoken,
            ninst,
            &mut seen,
            &mut |score, start, size| {
                if score > best_score {
                    best_score = score;
                    best_col = i;
                    best_start = start;
                    ncolsize = size;
                }
                best_score
            },
        );
        match r {
            Ok(()) => rc = Ok(()),
            Err((stop, e)) => {
                rc = Err(e);
                if stop {
                    break;
                }
            }
        }
    }
    rc?;
    let input = api.column_text(best_col)?;
    if ncolsize == 0 {
        ncolsize = api.column_size(best_col)?;
    }
    let Some(input) = input else {
        return Ok(Value::Null);
    };
    let iter = CInstIter::new(api, best_col)?;
    let mut h = Highlight {
        range_start: best_start,
        range_end: best_start + ntoken - 1,
        open: &open,
        close: &close,
        input: &input,
        iter,
        pos: 0,
        off: 0,
        is_open: false,
        out: Vec::new(),
    };
    if best_start > 0 {
        h.out.extend_from_slice(&ellips);
    }
    while h.iter.start >= 0 && h.iter.start < best_start {
        h.iter.next(api)?;
    }
    let locale = api.column_locale(best_col)?;
    run_highlight(api, &mut h, &input, locale.as_deref())?;
    if h.is_open {
        h.out.extend_from_slice(&close);
    }
    if h.range_end >= ncolsize - 1 {
        let off = h.off;
        h.append(off, input.len());
    } else {
        h.out.extend_from_slice(&ellips);
    }
    Ok(Value::build_text(
        String::from_utf8_lossy(&h.out).into_owned(),
    ))
}

/// Per-query bm25 data, kept as auxdata.
struct Bm25Data {
    avgdl: f64,
    idf: Vec<f64>,
}

fn bm25(api: &mut dyn AuxApi, args: &[Value]) -> ApiResult<Value> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    if api
        .auxdata()
        .and_then(|d| d.downcast_ref::<Bm25Data>())
        .is_none()
    {
        let nphrase = api.phrase_count();
        let nrow = api.row_count()?;
        let ntoken = api.column_total_size(-1)?;
        let avgdl = ntoken as f64 / nrow as f64;
        let mut idf = Vec::with_capacity(nphrase.max(0) as usize);
        for i in 0..nphrase {
            let nhit = api.query_phrase_count(i)?;
            let mut v = (((nrow - nhit) as f64 + 0.5) / (nhit as f64 + 0.5)).ln();
            if v <= 0.0 {
                v = 1e-6;
            }
            idf.push(v);
        }
        api.set_auxdata(Box::new(Bm25Data { avgdl, idf }));
    }
    let ninst = api.inst_count()?;
    let nphrase = api.phrase_count().max(0) as usize;
    let mut freq = vec![0.0f64; nphrase];
    for i in 0..ninst {
        let (ip, ic, _io) = api.inst(i)?;
        let w = if (args.len() as i32) > ic {
            value_double(&args[ic as usize])
        } else {
            1.0
        };
        freq[ip as usize] += w;
    }
    let d = api.column_size(-1)? as f64;
    let data = api
        .auxdata()
        .and_then(|d| d.downcast_ref::<Bm25Data>())
        .expect("bm25 data");
    let mut score = 0.0;
    for (i, f) in freq.iter().enumerate() {
        score += data.idf[i] * ((f * (K1 + 1.0)) / (f + K1 * (1.0 - B + B * d / data.avgdl)));
    }
    Ok(Value::from_f64(-1.0 * score))
}

fn get_locale(api: &mut dyn AuxApi, args: &[Value]) -> ApiResult<Value> {
    if args.len() != 1 {
        return Err(ApiError::Err(err(
            "wrong number of arguments to function fts5_get_locale()",
        )));
    }
    if super::config::numeric_int(&args[0]).is_none() {
        return Err(ApiError::Err(err(
            "non-integer argument passed to function fts5_get_locale()",
        )));
    }
    let col = clamped_int(&args[0]);
    if col < 0 || col >= api.column_count() {
        return Err(ApiError::Code(RANGE));
    }
    let loc = api.column_locale(col)?;
    Ok(match loc {
        Some(l) => Value::build_text(String::from_utf8_lossy(&l).into_owned()),
        None => Value::Null,
    })
}
