//! Virtual table planning, as SQLite's where.c does it: the constraints
//! handed to xBestIndex, the sequence of xBestIndex calls, and the plans
//! ("loops") those calls return.

use crate::native_ext::{
    index_op, IndexConstraint, IndexOrderBy, IndexPlan, IndexRequest, INDEX_SCAN_UNIQUE,
};
use crate::schema::Table;
use crate::translate::collate::{
    comparison_collation, expr_collation, CollationSeq, TextCollation,
};
use crate::translate::emitter::Resolver;
use crate::translate::expr::{sanitize_string, truth_test_rhs};
use crate::translate::expr_context::ExprContext;
use crate::translate::plan::{
    GroupBy, JoinedTable, NonFromClauseSubquery, ResultSetColumn, TableReferences, WhereTerm,
};
use crate::translate::planner::table_mask_from_expr;
use crate::util::exprs_are_equivalent;
use crate::vtab::VirtualTable;
use crate::{LimboError, Result, Value};
use std::sync::Arc;
use turso_parser::ast::{self, Expr, LikeOperator, Operator, SortOrder};

/// Where a constraint's value comes from.
#[derive(Debug, Clone)]
pub enum VtabRhs {
    Expr(Expr),
    /// `IS NULL` / `IS NOT NULL`.
    None,
    /// The values of an `IN (...)` list.
    InList(Vec<Expr>),
    /// An `IN (SELECT ...)`: the subquery result expression.
    InSubquery(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtabTermKind {
    /// The WHERE term itself, possibly commuted.
    Direct,
    /// A term SQLite derives from a WHERE term (commuted copy, `!=`, `LIKE`,
    /// LIKE prefix bound, vector component). Omitting every derived term of
    /// a WHERE term omits the WHERE term.
    Derived,
    Limit,
    Offset,
}

/// One entry of `aConstraint`.
#[derive(Debug, Clone)]
pub struct VtabTerm {
    /// The WHERE term; unused for LIMIT and OFFSET.
    pub where_idx: usize,
    pub kind: VtabTermKind,
    pub column: i32,
    pub op: u8,
    /// The constrained column, for the per-value IN check.
    pub lhs: Expr,
    pub rhs: VtabRhs,
    /// Tables (by join position) the value needs.
    pub prereq: u64,
    pub collation: String,
    pub rhs_value: Option<Value>,
    /// The constraint may not be omitted (a vector inequality).
    pub no_omit: bool,
    /// A case-insensitive LIKE: its prefix bounds never omit the LIKE.
    pub like_parent: bool,
}

impl VtabTerm {
    pub fn is_in(&self) -> bool {
        matches!(self.rhs, VtabRhs::InList(_) | VtabRhs::InSubquery(_))
    }

    fn is_limit(&self) -> bool {
        matches!(self.kind, VtabTermKind::Limit | VtabTermKind::Offset)
    }
}

/// A plan returned by one xBestIndex call.
#[derive(Debug, Clone)]
pub struct VtabLoop {
    /// Tables (by join position) the plan needs to run first.
    pub prereq: u64,
    pub plan: IndexPlan,
    /// Term per xFilter argument, in argv order.
    pub args: Vec<usize>,
    /// Per argument: the planner may skip checking the term.
    pub omit: Vec<bool>,
    /// Per argument: an IN passed whole (`sqlite3_vtab_in`).
    pub in_all_at_once: Vec<bool>,
    /// The module omits OFFSET, so the statement must not skip rows.
    pub omit_offset: bool,
    pub order_consumed: bool,
    pub unique: bool,
}

/// Everything planned for one virtual table in one query.
#[derive(Debug)]
pub struct VtabCandidates {
    pub terms: Vec<VtabTerm>,
    pub loops: Vec<VtabLoop>,
    /// The ORDER BY handed to xBestIndex.
    pub order_by: Vec<IndexOrderBy>,
}

/// What the query does with its rows, for `aOrderBy`, `sqlite3_vtab_distinct`
/// and the LIMIT/OFFSET constraints.
#[derive(Debug, Clone, Copy, Default)]
pub struct VtabQueryShape {
    pub distinct: bool,
    pub aggregate: bool,
    /// UPDATE reads every column (`colUsed` is all ones).
    pub all_columns: bool,
}

pub(crate) struct VtabPlanInput<'a> {
    pub table_references: &'a TableReferences,
    pub table_index: usize,
    pub where_clause: &'a [WhereTerm],
    pub subqueries: &'a [NonFromClauseSubquery],
    pub resolver: &'a Resolver<'a>,
    pub order_by: &'a [(Box<Expr>, SortOrder, Option<ast::NullsOrder>)],
    pub group_by: Option<&'a GroupBy>,
    pub result_columns: &'a [ResultSetColumn],
    pub limit: Option<&'a Expr>,
    pub offset: Option<&'a Expr>,
    pub shape: VtabQueryShape,
    /// Plan for the unmatched rows of a RIGHT JOIN: no IN loops.
    pub exclude_in: bool,
}

fn collation_name(c: CollationSeq) -> String {
    match c {
        CollationSeq::Text(TextCollation::Binary) | CollationSeq::Type(_) => "BINARY".into(),
        CollationSeq::Text(TextCollation::NoCase) => "NOCASE".into(),
        CollationSeq::Text(TextCollation::Rtrim) => "RTRIM".into(),
        CollationSeq::Text(t) => t.name(),
    }
}

fn skip_collate(mut e: &Expr) -> &Expr {
    loop {
        match e {
            Expr::Collate(inner, _) => e = inner,
            Expr::Parenthesized(v) if v.len() == 1 => e = &v[0],
            _ => return e,
        }
    }
}

fn unparen(mut e: &Expr) -> &Expr {
    while let Expr::Parenthesized(v) = e {
        if v.len() != 1 {
            break;
        }
        e = &v[0];
    }
    e
}

fn vector_items(e: &Expr) -> Option<&[Box<Expr>]> {
    match e {
        Expr::Parenthesized(v) if v.len() > 1 => Some(v),
        _ => None,
    }
}

/// `sqlite3ValueFromExpr` for a constraint's right-hand side.
fn literal_value(e: &Expr) -> Option<Value> {
    match e {
        Expr::Literal(lit) => match lit {
            ast::Literal::Numeric(n) => crate::util::parse_numeric_literal(n).ok(),
            ast::Literal::String(s) => Some(Value::build_text(sanitize_string(s))),
            ast::Literal::Blob(b) => {
                let hex = ast::blob_literal_hex(b);
                let bytes = (0..hex.len() / 2)
                    .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
                    .collect::<Option<Vec<u8>>>()?;
                Some(Value::from_blob(bytes))
            }
            ast::Literal::Null => Some(Value::Null),
            ast::Literal::True => Some(Value::from_i64(1)),
            ast::Literal::False => Some(Value::from_i64(0)),
            _ => None,
        },
        Expr::Unary(ast::UnaryOperator::Positive, inner) => literal_value(inner),
        Expr::Unary(ast::UnaryOperator::Negative, inner) => match inner.as_ref() {
            Expr::Literal(ast::Literal::Numeric(n)) => {
                match crate::util::parse_numeric_literal(n).ok()? {
                    Value::Numeric(crate::Numeric::Integer(i)) => {
                        Some(Value::from_i64(i.checked_neg()?))
                    }
                    Value::Numeric(crate::Numeric::Float(f)) => {
                        Some(Value::from_f64(-f64::from(f)))
                    }
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// `sqlite3ExprIsInteger` for LIMIT and OFFSET.
fn literal_integer(e: &Expr) -> Option<i64> {
    match literal_value(e)? {
        Value::Numeric(crate::Numeric::Integer(i)) if (0..=i32::MAX as i64).contains(&i) => Some(i),
        _ => None,
    }
}

/// `sqlite3AtoF(z) > 0`: the whole text is a number.
fn looks_numeric(s: &str) -> bool {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let t = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mant, exp) = match t.find(['e', 'E']) {
        Some(i) => (&t[..i], Some(&t[i + 1..])),
        None => (t, None),
    };
    let digits = mant.chars().filter(|c| c.is_ascii_digit()).count();
    let dots = mant.chars().filter(|c| *c == '.').count();
    if digits == 0 || dots > 1 || mant.chars().any(|c| !c.is_ascii_digit() && c != '.') {
        return false;
    }
    match exp {
        None => true,
        Some(e) => {
            let e = e.strip_prefix(['+', '-']).unwrap_or(e);
            !e.is_empty() && e.chars().all(|c| c.is_ascii_digit())
        }
    }
}

struct Analyzer<'a> {
    input: &'a VtabPlanInput<'a>,
    table: &'a JoinedTable,
    /// Tables left of this one; ON terms of an outer join need them all.
    extra_right: u64,
}

/// A column of the table being planned: (column number, expression).
enum ColRef {
    Ours(i32),
    /// A column of another table in the FROM clause; `true` for a virtual table.
    Other(bool),
}

impl<'a> Analyzer<'a> {
    fn col(&self, e: &Expr) -> Option<ColRef> {
        let (table_id, column) = match skip_collate(e) {
            Expr::Column { table, column, .. } => (*table, *column as i32),
            Expr::RowId { table, .. } => (*table, -1),
            _ => return None,
        };
        if table_id == self.table.internal_id {
            return Some(ColRef::Ours(column));
        }
        let other = self
            .input
            .table_references
            .joined_tables()
            .iter()
            .find(|t| t.internal_id == table_id)?;
        Some(ColRef::Other(matches!(other.table, Table::Virtual(_))))
    }

    fn ours(&self, e: &Expr) -> Option<i32> {
        match self.col(e) {
            Some(ColRef::Ours(c)) => Some(c),
            _ => None,
        }
    }

    /// `ExprIsVtab`: a column of a virtual table (this one or another), with
    /// no COLLATE around it.
    fn is_vtab_col(&self, e: &Expr) -> bool {
        match e {
            Expr::Column { .. } | Expr::RowId { .. } => match self.col(e) {
                Some(ColRef::Ours(_)) => true,
                Some(ColRef::Other(v)) => v,
                None => false,
            },
            _ => false,
        }
    }

    fn mask(&self, e: &Expr) -> Result<u64> {
        let m = table_mask_from_expr(e, self.input.table_references, self.input.subqueries)?;
        Ok(m.iter()
            .filter(|i| *i < 64)
            .fold(0u64, |acc, i| acc | (1 << i)))
    }

    fn compare_collation(&self, l: &Expr, r: &Expr) -> Result<String> {
        let ctx = ExprContext::new(Some(self.input.table_references), self.input.resolver);
        Ok(collation_name(comparison_collation(l, r, &ctx)?))
    }

    #[allow(clippy::too_many_arguments)]
    fn term(
        &self,
        where_idx: usize,
        kind: VtabTermKind,
        column: i32,
        op: u8,
        lhs: &Expr,
        rhs: VtabRhs,
        prereq: u64,
        collation: String,
    ) -> VtabTerm {
        let rhs_value = match &rhs {
            VtabRhs::Expr(e) => literal_value(e),
            _ => None,
        };
        VtabTerm {
            where_idx,
            kind,
            column,
            op,
            lhs: lhs.clone(),
            rhs,
            prereq,
            collation,
            rhs_value,
            no_omit: false,
            like_parent: false,
        }
    }

    /// One comparison `l op r` (op among = < <= > >= IS), as `exprAnalyze`
    /// does for an indexable operator.
    #[allow(clippy::too_many_arguments)]
    fn comparison(
        &self,
        where_idx: usize,
        kind: VtabTermKind,
        l: &Expr,
        op: Operator,
        r: &Expr,
        out: &mut Vec<VtabTerm>,
        appended: &mut Vec<VtabTerm>,
        extra: u64,
    ) -> Result<()> {
        let code = |op: Operator| match op {
            Operator::Equals => index_op::EQ,
            Operator::Less => index_op::LT,
            Operator::LessEquals => index_op::LE,
            Operator::Greater => index_op::GT,
            Operator::GreaterEquals => index_op::GE,
            _ => index_op::IS,
        };
        let commute = |op: Operator| match op {
            Operator::Less => Operator::Greater,
            Operator::LessEquals => Operator::GreaterEquals,
            Operator::Greater => Operator::Less,
            Operator::GreaterEquals => Operator::LessEquals,
            o => o,
        };
        // Both sides on the same table: not indexable (`WO_EQUIV` only).
        let (lmask, rmask) = (self.mask(l)?, self.mask(r)?);
        if lmask & rmask != 0 {
            return Ok(());
        }
        let collation = self.compare_collation(l, r)?;
        let left = self.col(l);
        if let Some(ColRef::Ours(c)) = left {
            out.push(self.term(
                where_idx,
                kind,
                c,
                code(op),
                l,
                VtabRhs::Expr(r.clone()),
                self.mask(r)? | extra,
                collation.clone(),
            ));
        }
        if let Some(c) = self.ours(r) {
            let t = self.term(
                where_idx,
                if left.is_some() {
                    VtabTermKind::Derived
                } else {
                    kind
                },
                c,
                code(commute(op)),
                r,
                VtabRhs::Expr(l.clone()),
                self.mask(l)? | extra,
                collation,
            );
            if left.is_some() {
                appended.push(t);
            } else {
                out.push(t);
            }
        }
        Ok(())
    }

    /// `isAuxiliaryVtabOperator` for `!=`, `IS NOT` and `IS NOT NULL`.
    fn aux_ne(
        &self,
        where_idx: usize,
        op: u8,
        l: &Expr,
        r: Option<&Expr>,
        appended: &mut Vec<VtabTerm>,
        extra: u64,
    ) -> Result<()> {
        let mut res = 0;
        let (mut col, mut val) = (l, r);
        if self.is_vtab_col(l) {
            res += 1;
        }
        if let Some(rr) = r {
            if self.is_vtab_col(rr) {
                res += 1;
                col = rr;
                val = Some(l);
            }
        }
        for _ in 0..res {
            let val_mask = match val {
                Some(v) => self.mask(v)?,
                None => 0,
            };
            if val_mask & self.mask(col)? == 0 {
                if let Some(c) = self.ours(col) {
                    let rhs = match val {
                        Some(v) => VtabRhs::Expr(v.clone()),
                        None => VtabRhs::None,
                    };
                    appended.push(self.term(
                        where_idx,
                        VtabTermKind::Derived,
                        c,
                        op,
                        col,
                        rhs,
                        val_mask | extra,
                        "BINARY".into(),
                    ));
                }
            }
            if let Some(v) = val {
                (col, val) = (v, Some(col));
            }
        }
        Ok(())
    }

    /// The LIKE/GLOB prefix bounds of `isLikeOrGlob`, as two virtual terms.
    fn like_bounds(
        &self,
        where_idx: usize,
        op: LikeOperator,
        lhs: &Expr,
        pattern: &Expr,
        escape: Option<&Expr>,
        appended: &mut Vec<VtabTerm>,
        extra: u64,
    ) -> Result<()> {
        let (wc, no_case): ([u8; 3], bool) = match op {
            // Turso has no PRAGMA case_sensitive_like.
            LikeOperator::Like => ([b'%', b'_', 0], true),
            LikeOperator::Glob => ([b'*', b'?', b'['], false),
            _ => return Ok(()),
        };
        let esc = match escape {
            None => 0u8,
            Some(Expr::Literal(ast::Literal::String(s))) => {
                let s = sanitize_string(s);
                if s.len() != 1 || s.as_bytes()[0] == wc[0] || s.as_bytes()[0] == wc[1] {
                    return Ok(());
                }
                s.as_bytes()[0]
            }
            Some(_) => return Ok(()),
        };
        let Expr::Literal(ast::Literal::String(pat)) = skip_collate(pattern) else {
            return Ok(());
        };
        let z = sanitize_string(pat);
        let z = z.as_bytes();
        let mut cnt = 0;
        while cnt < z.len() {
            let c = z[cnt];
            if c == wc[0] || c == wc[1] || (wc[2] != 0 && c == wc[2]) {
                break;
            }
            cnt += 1;
            if esc != 0 && c == esc && cnt < z.len() && z[cnt] > 0 && z[cnt] < 0x80 {
                cnt += 1;
            } else if c >= 0x80 {
                let rest = std::str::from_utf8(&z[cnt - 1..]).ok();
                match rest.and_then(|r| r.chars().next()) {
                    Some(ch) if ch != '\u{fffd}' && c != 0xff => cnt += ch.len_utf8() - 1,
                    _ => {
                        cnt -= 1;
                        break;
                    }
                }
            }
        }
        if !((cnt > 1 || (cnt > 0 && (esc == 0 || z[0] != esc))) && z[cnt - 1] != 255) {
            return Ok(());
        }
        let mut prefix: Vec<u8> = Vec::with_capacity(cnt);
        let mut i = 0;
        while i < cnt {
            if esc != 0 && z[i] == esc {
                i += 1;
            }
            if i < cnt {
                prefix.push(z[i]);
            }
            i += 1;
        }
        let Ok(text) = String::from_utf8(prefix.clone()) else {
            return Ok(());
        };
        // A vtab column might be numeric: the bounds must not look like numbers.
        let mut is_num = looks_numeric(&text);
        if !is_num {
            if prefix.len() == 1 && prefix[0] == b'-' {
                is_num = true;
            } else {
                let mut bumped = prefix.clone();
                *bumped.last_mut().unwrap() = bumped.last().unwrap().wrapping_add(1);
                is_num = String::from_utf8(bumped).is_ok_and(|s| looks_numeric(&s));
            }
        }
        if is_num {
            return Ok(());
        }
        let mut lower = prefix.clone();
        let mut upper = prefix;
        if no_case {
            lower.make_ascii_uppercase();
            upper.make_ascii_lowercase();
        }
        let mut k = upper.len() - 1;
        while upper[k] == 0xBF && k > 0 {
            upper[k] = 0x80;
            k -= 1;
        }
        upper[k] = upper[k].wrapping_add(1);
        let Some(c) = self.ours(lhs) else {
            return Ok(());
        };
        let coll = if no_case { "NOCASE" } else { "BINARY" };
        for (op, bytes) in [(index_op::GE, lower), (index_op::LT, upper)] {
            let s = String::from_utf8_lossy(&bytes).into_owned();
            let lit = Expr::Literal(ast::Literal::String(format!("'{}'", s.replace('\'', "''"))));
            let mut t = self.term(
                where_idx,
                VtabTermKind::Derived,
                c,
                op,
                lhs,
                VtabRhs::Expr(lit),
                extra,
                coll.into(),
            );
            t.like_parent = no_case;
            appended.push(t);
        }
        Ok(())
    }

    /// Function-style operators: `like(p, col)`, `col MATCH x`, `f(col, x)`.
    fn aux_function(
        &self,
        where_idx: usize,
        name: &str,
        args: &[&Expr],
        appended: &mut Vec<VtabTerm>,
        extra: u64,
    ) -> Result<()> {
        if args.len() != 2 {
            return Ok(());
        }
        let like_op = match name.to_ascii_lowercase().as_str() {
            "match" => Some(index_op::MATCH),
            "glob" => Some(index_op::GLOB),
            "like" => Some(index_op::LIKE),
            "regexp" => Some(index_op::REGEXP),
            _ => None,
        };
        if let (Some(op), true) = (like_op, self.is_vtab_col(args[1])) {
            return self.push_aux(where_idx, op, args[1], args[0], appended, extra);
        }
        if self.is_vtab_col(args[0]) {
            if let (Some(_), Table::Virtual(vtab)) = (self.ours(args[0]), &self.table.table) {
                if let Some(op) = vtab.find_function_op(2, name) {
                    if op >= i32::from(index_op::FUNCTION) && op <= 255 {
                        return self
                            .push_aux(where_idx, op as u8, args[0], args[1], appended, extra);
                    }
                }
            }
        }
        Ok(())
    }

    fn push_aux(
        &self,
        where_idx: usize,
        op: u8,
        col: &Expr,
        val: &Expr,
        appended: &mut Vec<VtabTerm>,
        extra: u64,
    ) -> Result<()> {
        let val_mask = self.mask(val)?;
        if val_mask & self.mask(col)? != 0 {
            return Ok(());
        }
        if let Some(c) = self.ours(col) {
            appended.push(self.term(
                where_idx,
                VtabTermKind::Derived,
                c,
                op,
                col,
                VtabRhs::Expr(val.clone()),
                val_mask | extra,
                "BINARY".into(),
            ));
        }
        Ok(())
    }

    fn analyze(
        &self,
        where_idx: usize,
        expr: &Expr,
        out: &mut Vec<VtabTerm>,
        appended: &mut Vec<VtabTerm>,
        extra: u64,
    ) -> Result<()> {
        match unparen(expr) {
            Expr::Binary(l, op, r)
                if matches!(
                    op,
                    Operator::Equals
                        | Operator::Less
                        | Operator::LessEquals
                        | Operator::Greater
                        | Operator::GreaterEquals
                        | Operator::Is
                ) =>
            {
                let (l, r) = (unparen(l), unparen(r));
                if *op == Operator::Is && matches!(r, Expr::Literal(ast::Literal::Null)) {
                    return self.is_null(where_idx, l, out);
                }
                if *op == Operator::Is && truth_test_rhs(r).is_some() {
                    return Ok(());
                }
                match (vector_items(l), vector_items(r)) {
                    (None, None) => self.comparison(
                        where_idx,
                        VtabTermKind::Direct,
                        l,
                        *op,
                        r,
                        out,
                        appended,
                        extra,
                    ),
                    (Some(lv), Some(rv))
                        if lv.len() == rv.len()
                            && matches!(op, Operator::Equals | Operator::Is) =>
                    {
                        // Vector equality: one term per component, appended.
                        for (a, b) in lv.iter().zip(rv.iter()) {
                            let mut tmp = Vec::new();
                            let mut tmp_app = Vec::new();
                            self.comparison(
                                where_idx,
                                VtabTermKind::Derived,
                                a,
                                *op,
                                b,
                                &mut tmp,
                                &mut tmp_app,
                                extra,
                            )?;
                            appended.extend(tmp);
                            appended.extend(tmp_app);
                        }
                        Ok(())
                    }
                    (Some(lv), Some(rv)) if lv.len() == rv.len() && *op != Operator::Equals => {
                        // Vector inequality: the first column, never omitted.
                        if let Some(c) = self.ours(&lv[0]) {
                            let code = match op {
                                Operator::Less | Operator::LessEquals => index_op::LE,
                                _ => index_op::GE,
                            };
                            let mut t = self.term(
                                where_idx,
                                VtabTermKind::Direct,
                                c,
                                code,
                                &lv[0],
                                VtabRhs::Expr(*rv[0].clone()),
                                self.mask(r)? | extra,
                                self.compare_collation(&lv[0], &rv[0])?,
                            );
                            t.rhs_value = None;
                            t.no_omit = true;
                            out.push(t);
                        }
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
            Expr::Binary(l, op @ (Operator::NotEquals | Operator::IsNot), r) => {
                let (l, r) = (unparen(l), unparen(r));
                if *op == Operator::IsNot && matches!(r, Expr::Literal(ast::Literal::Null)) {
                    return self.aux_ne(where_idx, index_op::ISNOTNULL, l, None, appended, extra);
                }
                if *op == Operator::IsNot && truth_test_rhs(r).is_some() {
                    return Ok(());
                }
                if vector_items(l).is_some() || vector_items(r).is_some() {
                    return Ok(());
                }
                let code = if *op == Operator::NotEquals {
                    index_op::NE
                } else {
                    index_op::ISNOT
                };
                self.aux_ne(where_idx, code, l, Some(r), appended, extra)
            }
            Expr::NotNull(l) => self.aux_ne(
                where_idx,
                index_op::ISNOTNULL,
                unparen(l),
                None,
                appended,
                extra,
            ),
            Expr::IsNull(l) => self.is_null(where_idx, unparen(l), out),
            Expr::Like {
                lhs,
                not: false,
                op,
                rhs,
                escape,
            } => {
                self.like_bounds(where_idx, *op, lhs, rhs, escape.as_deref(), appended, extra)?;
                if escape.is_some() {
                    return Ok(());
                }
                let name = match op {
                    LikeOperator::Like => "like",
                    LikeOperator::Glob => "glob",
                    LikeOperator::Match => "match",
                    LikeOperator::Regexp => "regexp",
                };
                self.aux_function(where_idx, name, &[rhs, lhs], appended, extra)
            }
            Expr::FunctionCall {
                name,
                args,
                distinctness: None,
                order_by,
                filter_over,
                ..
            } if order_by.is_empty()
                && filter_over.filter_clause.is_none()
                && filter_over.over_clause.is_none() =>
            {
                let args: Vec<&Expr> = args.iter().map(|a| a.as_ref()).collect();
                self.aux_function(where_idx, name.as_str(), &args, appended, extra)
            }
            Expr::InList {
                lhs,
                not: false,
                rhs,
            } => {
                let l = unparen(lhs);
                if vector_items(l).is_some() || rhs.is_empty() {
                    return Ok(());
                }
                let Some(c) = self.ours(l) else {
                    return Ok(());
                };
                let mut prereq = extra;
                for v in rhs {
                    prereq |= self.mask(v)?;
                }
                let ctx = ExprContext::new(Some(self.input.table_references), self.input.resolver);
                let collation = collation_name(expr_collation(l, &ctx)?);
                if rhs.len() == 1 && prereq == extra {
                    out.push(self.term(
                        where_idx,
                        VtabTermKind::Direct,
                        c,
                        index_op::EQ,
                        l,
                        VtabRhs::Expr(*rhs[0].clone()),
                        prereq,
                        collation,
                    ));
                } else {
                    out.push(self.term(
                        where_idx,
                        VtabTermKind::Direct,
                        c,
                        index_op::EQ,
                        l,
                        VtabRhs::InList(rhs.iter().map(|e| *e.clone()).collect()),
                        prereq,
                        collation,
                    ));
                }
                Ok(())
            }
            e @ Expr::SubqueryResult {
                lhs: Some(lhs),
                not_in: false,
                query_type: ast::SubqueryType::In { .. },
                ..
            } => {
                let l = unparen(lhs);
                if vector_items(l).is_some() {
                    return Ok(());
                }
                let Some(c) = self.ours(l) else {
                    return Ok(());
                };
                let mut without_lhs = e.clone();
                if let Expr::SubqueryResult { lhs, .. } = &mut without_lhs {
                    *lhs = None;
                }
                let prereq = self.mask(&without_lhs)? | extra;
                let ctx = ExprContext::new(Some(self.input.table_references), self.input.resolver);
                let collation = collation_name(expr_collation(l, &ctx)?);
                out.push(self.term(
                    where_idx,
                    VtabTermKind::Direct,
                    c,
                    index_op::EQ,
                    l,
                    VtabRhs::InSubquery(e.clone()),
                    prereq,
                    collation,
                ));
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn is_null(&self, where_idx: usize, l: &Expr, out: &mut Vec<VtabTerm>) -> Result<()> {
        let Some(c) = self.ours(l) else {
            return Ok(());
        };
        // `x IS NULL` on a column that cannot be NULL is folded to false.
        let notnull = c < 0
            || self
                .table
                .columns()
                .get(c as usize)
                .is_some_and(|col| col.notnull());
        if notnull {
            return Ok(());
        }
        let ctx = ExprContext::new(Some(self.input.table_references), self.input.resolver);
        let collation = collation_name(expr_collation(l, &ctx)?);
        out.push(self.term(
            where_idx,
            VtabTermKind::Direct,
            c,
            index_op::ISNULL,
            l,
            VtabRhs::None,
            0,
            collation,
        ));
        Ok(())
    }
}

/// Tables (by join position) a virtual table must follow, and tables that
/// must follow it, as `whereLoopAddAll` computes `mPrereq` and `mUnusable`.
fn prereq_masks(tables: &TableReferences, index: usize) -> (u64, u64) {
    let joined = tables.joined_tables();
    let boundary = |t: &JoinedTable| {
        t.join_info
            .as_ref()
            .is_some_and(|j| j.is_outer() || j.no_reorder)
    };
    let mut prereq = 0u64;
    let mut prior = 0u64;
    let mut has_right_join = false;
    for (k, t) in joined.iter().enumerate().take(index + 1) {
        if boundary(t) || tables.is_left_of_right_or_full_join(t.internal_id) {
            if tables.is_left_of_right_or_full_join(t.internal_id) {
                has_right_join = true;
            }
            prereq |= prior;
        } else if !has_right_join {
            prereq = 0;
        }
        if k < 64 {
            prior |= 1 << k;
        }
    }
    let mut unusable = 0u64;
    for (k, t) in joined.iter().enumerate().skip(index + 1) {
        if (unusable != 0 || boundary(t)) && k < 64 {
            unusable |= 1 << k;
        }
    }
    (prereq, unusable)
}

/// The constraint list handed to xBestIndex (`allocateIndexInfo`).
pub(crate) fn collect_terms(input: &VtabPlanInput) -> Result<(Vec<VtabTerm>, bool)> {
    let table = &input.table_references.joined_tables()[input.table_index];
    let id = table.internal_id;
    let outerish = table.join_info.as_ref().is_some_and(|j| j.is_outer())
        || input
            .table_references
            .is_at_or_left_of_right_or_full_join(id);
    let left_tables: u64 = (0..input.table_index.min(64)).fold(0, |acc, i| acc | (1 << i));
    let analyzer = Analyzer {
        input,
        table,
        extra_right: left_tables,
    };
    let mut direct = Vec::new();
    let mut appended: Vec<Vec<VtabTerm>> = vec![Vec::new(); input.where_clause.len()];
    // LIMIT needs every WHERE term passed through without a prerequisite.
    let mut all_passed = true;
    for (i, term) in input.where_clause.iter().enumerate() {
        if term.consumed {
            continue;
        }
        let origin = term.origin.join_origin();
        if let Some(outer) = origin.and_then(|o| o.outer_table()) {
            if outer != id {
                all_passed = false;
                continue;
            }
        }
        if outerish && origin.is_none_or(|o| o.right_table() != id) {
            all_passed = false;
            continue;
        }
        let extra = if term.origin.is_outer_join() {
            analyzer.extra_right
        } else {
            0
        };
        let before = direct.len();
        analyzer.analyze(i, &term.expr, &mut direct, &mut appended[i], extra)?;
        let produced = direct[before..].iter().chain(appended[i].iter());
        let mut any = false;
        for t in produced {
            any = true;
            if t.prereq != 0 {
                all_passed = false;
            }
        }
        if !any {
            all_passed = false;
        }
    }
    let mut terms = direct;
    for list in appended.into_iter().rev() {
        terms.extend(list);
    }
    let (_, unusable) = prereq_masks(input.table_references, input.table_index);
    terms.retain(|t| t.prereq & unusable == 0);
    Ok((terms, all_passed))
}

/// The ORDER BY list to pass and `sqlite3_vtab_distinct`.
fn order_by_for(input: &VtabPlanInput) -> Result<(Vec<IndexOrderBy>, i32, bool)> {
    let table = &input.table_references.joined_tables()[input.table_index];
    let shape = input.shape;
    let result_exprs: Vec<(&Expr, SortOrder, Option<ast::NullsOrder>)> = input
        .result_columns
        .iter()
        .map(|c| (&c.expr, SortOrder::Asc, None))
        .collect();
    let order_exprs: Vec<(&Expr, SortOrder, Option<ast::NullsOrder>)> = input
        .order_by
        .iter()
        .map(|(e, o, n)| (e.as_ref(), *o, *n))
        .collect();
    let same = |a: &[(&Expr, SortOrder, Option<ast::NullsOrder>)],
                b: &[(&Expr, SortOrder, Option<ast::NullsOrder>)]| {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| x.1 == y.1 && x.2 == y.2 && exprs_are_equivalent(x.0, y.0))
    };
    const DISTINCTBY: u8 = 1;
    const GROUPBY: u8 = 2;
    const WANT_DISTINCT: u8 = 4;
    const SORTBYGROUP: u8 = 8;
    let (list, flags): (Vec<(&Expr, SortOrder, Option<ast::NullsOrder>)>, u8) =
        if let Some(group_by) = input.group_by {
            let list: Vec<_> = group_by
                .exprs
                .iter()
                .map(|e| (e, SortOrder::Asc, None))
                .collect();
            let sbg = if same(&order_exprs, &list) {
                SORTBYGROUP
            } else {
                0
            };
            (list, GROUPBY | sbg)
        } else if shape.distinct && !shape.aggregate {
            if order_exprs.is_empty() {
                (result_exprs, DISTINCTBY)
            } else if same(&order_exprs, &result_exprs) {
                (result_exprs, DISTINCTBY | SORTBYGROUP)
            } else {
                (order_exprs, WANT_DISTINCT)
            }
        } else if shape.aggregate {
            (Vec::new(), 0)
        } else {
            (order_exprs, 0)
        };
    let mut out = Vec::new();
    let mut all = !list.is_empty();
    for (e, order, nulls) in &list {
        if e.is_constant(input.resolver) {
            continue;
        }
        let default_nulls = match (order, nulls) {
            (_, None) => true,
            (SortOrder::Asc, Some(ast::NullsOrder::First)) => true,
            (SortOrder::Desc, Some(ast::NullsOrder::Last)) => true,
            _ => false,
        };
        if !default_nulls {
            all = false;
            break;
        }
        let column = match e {
            Expr::Column {
                table: t, column, ..
            } if *t == table.internal_id => Some(*column as i32),
            Expr::RowId { table: t, .. } if *t == table.internal_id => Some(-1),
            Expr::Collate(inner, name) => match inner.as_ref() {
                Expr::Column {
                    table: t, column, ..
                } if *t == table.internal_id => {
                    let col_coll = table
                        .columns()
                        .get(*column)
                        .map(|c| collation_name(c.collation()))
                        .unwrap_or_else(|| "BINARY".into());
                    name.as_str()
                        .eq_ignore_ascii_case(&col_coll)
                        .then_some(*column as i32)
                }
                Expr::RowId { table: t, .. } if *t == table.internal_id => Some(-1),
                _ => None,
            },
            _ => None,
        };
        let Some(column) = column else {
            all = false;
            break;
        };
        out.push(IndexOrderBy {
            column,
            desc: *order == SortOrder::Desc,
        });
    }
    // LIMIT also needs every ORDER BY term to be a plain column.
    let plain = input.order_by.iter().all(|(e, o, n)| {
        matches!(e.as_ref(), Expr::Column { table: t, .. } | Expr::RowId { table: t, .. } if *t == table.internal_id)
            && match (o, n) {
                (_, None) => true,
                (SortOrder::Asc, Some(ast::NullsOrder::First)) => true,
                (SortOrder::Desc, Some(ast::NullsOrder::Last)) => true,
                _ => false,
            }
    });
    if !all {
        return Ok((Vec::new(), 0, plain));
    }
    let sbg = (flags & SORTBYGROUP != 0) as i32;
    let distinct = if flags & DISTINCTBY != 0 {
        2 + sbg
    } else if flags & GROUPBY != 0 {
        1 - sbg
    } else if flags & WANT_DISTINCT != 0 {
        3
    } else {
        0
    };
    Ok((out, distinct, plain))
}

use super::Optimizable;

fn col_used(table: &JoinedTable, all: bool) -> u64 {
    if all {
        return u64::MAX;
    }
    let mut m = 0u64;
    for i in table.col_used_mask.iter() {
        m |= 1 << i.min(63);
    }
    m
}

struct LoopBuilder<'a> {
    vtab: &'a VirtualTable,
    table_name: &'a str,
    terms: &'a [VtabTerm],
    req: IndexRequest,
    loops: Vec<VtabLoop>,
}

impl LoopBuilder<'_> {
    /// `whereLoopAddVirtualOne`. Returns (uses a per-value IN, retry without
    /// LIMIT, the prerequisites seen), as SQLite leaves them in `pNew`.
    fn add_one(
        &mut self,
        m_prereq: u64,
        m_usable: u64,
        exclude_in: bool,
        allow_limit: bool,
    ) -> Result<(bool, bool, u64)> {
        for (c, t) in self.req.constraints.iter_mut().zip(self.terms) {
            c.usable = t.prereq & m_usable == t.prereq
                && !(exclude_in && t.is_in())
                && (allow_limit || !t.is_limit());
        }
        let Some(plan) = self.vtab.best_index_plan(&self.req)? else {
            return Ok((false, false, m_prereq));
        };
        let malfunction =
            || LimboError::ParseError(format!("{}.xBestIndex malfunction", self.table_name));
        let n = self.terms.len();
        if plan.usages.len() != n {
            return Err(malfunction());
        }
        let mut args: Vec<Option<usize>> = vec![None; n];
        let mut omit = vec![false; n];
        let mut handle_in = vec![false; n];
        let mut prereq = m_prereq;
        let mut uses_in = false;
        let mut order_consumed = plan.order_by_consumed;
        let mut flags = plan.idx_flags;
        let mut omit_offset = false;
        let mut count = 0;
        for i in 0..n {
            let Some(argv) = plan.usages[i].argv_index else {
                continue;
            };
            let slot = argv as usize;
            if slot == 0 || slot > n || args[slot - 1].is_some() || !self.req.constraints[i].usable
            {
                return Err(malfunction());
            }
            let t = &self.terms[i];
            prereq |= t.prereq;
            args[slot - 1] = Some(i);
            count = count.max(slot);
            if plan.usages[i].omit {
                // SQLite's omit mask covers the first 16 constraints and arguments.
                if i < 16 && slot <= 16 && !t.no_omit {
                    omit[slot - 1] = true;
                }
                if t.kind == VtabTermKind::Offset {
                    omit_offset = true;
                }
            }
            if i < 32 && t.is_in() && plan.in_all_at_once.get(i).copied().unwrap_or(false) {
                handle_in[slot - 1] = true;
            } else if t.is_in() {
                // Values arrive one per xFilter: no order, no single row.
                order_consumed = false;
                flags &= !INDEX_SCAN_UNIQUE;
                uses_in = true;
            }
            if t.is_limit() && (uses_in || (0..i).any(|j| plan.usages[j].argv_index.is_none())) {
                return Ok((uses_in, true, prereq));
            }
        }
        let mut arg_terms = Vec::with_capacity(count);
        for a in args.iter().take(count) {
            arg_terms.push(a.ok_or_else(malfunction)?);
        }
        omit.truncate(count);
        handle_in.truncate(count);
        self.loops.push(VtabLoop {
            prereq,
            unique: flags & INDEX_SCAN_UNIQUE != 0,
            order_consumed: order_consumed && !self.req.order_by.is_empty(),
            plan,
            args: arg_terms,
            omit,
            in_all_at_once: handle_in,
            omit_offset,
        });
        Ok((uses_in, false, prereq))
    }
}

/// Runs SQLite's sequence of xBestIndex calls (`whereLoopAddVirtual`).
pub(crate) fn plan_vtab(input: &VtabPlanInput) -> Result<Option<Arc<VtabCandidates>>> {
    let table = &input.table_references.joined_tables()[input.table_index];
    let Table::Virtual(vtab) = &table.table else {
        return Ok(None);
    };
    let (mut terms, all_passed) = collect_terms(input)?;
    let (order_by, distinct, plain_order) = order_by_for(input)?;
    if let Some(limit) = input.limit {
        let single = input.table_references.joined_tables().len() == 1;
        let shape = input.shape;
        if single
            && all_passed
            && plain_order
            && input.group_by.is_none()
            && !shape.distinct
            && !shape.aggregate
        {
            let lhs = Expr::Literal(ast::Literal::Null);
            let mut add = |kind, op, e: &Expr| {
                terms.push(VtabTerm {
                    where_idx: usize::MAX,
                    kind,
                    column: 0,
                    op,
                    lhs: lhs.clone(),
                    rhs: VtabRhs::Expr(e.clone()),
                    prereq: 0,
                    collation: "BINARY".into(),
                    rhs_value: literal_integer(e).map(Value::from_i64),
                    no_omit: false,
                    like_parent: false,
                });
            };
            if let Some(offset) = input.offset {
                add(VtabTermKind::Offset, index_op::OFFSET, offset);
            }
            add(VtabTermKind::Limit, index_op::LIMIT, limit);
        }
    }
    let req = IndexRequest {
        constraints: terms
            .iter()
            .map(|t| IndexConstraint {
                column: t.column,
                op: t.op,
                usable: false,
                collation: t.collation.clone(),
                rhs: t.rhs_value.clone(),
                is_in: t.is_in(),
            })
            .collect(),
        order_by: order_by.clone(),
        // Columns in constraints count too (table-function arguments).
        col_used: terms
            .iter()
            .filter(|t| !t.is_limit() && t.column >= 0)
            .fold(col_used(table, input.shape.all_columns), |m, t| {
                m | (1 << t.column.min(63))
            }),
        distinct,
    };
    let (m_prereq, _) = prereq_masks(input.table_references, input.table_index);
    let mut b = LoopBuilder {
        vtab,
        table_name: &table.table.get_name().to_string(),
        terms: &terms,
        req,
        loops: Vec::new(),
    };
    let all = u64::MAX;
    let ex_in = input.exclude_in;
    let (mut b_in, retry, mut got) = b.add_one(m_prereq, all, ex_in, true)?;
    if retry {
        (b_in, _, got) = b.add_one(m_prereq, all, ex_in, false)?;
    }
    let m_best = got & !m_prereq;
    if m_best != 0 || b_in {
        let mut seen_zero = false;
        let mut seen_zero_no_in = false;
        let mut m_best_no_in = 0;
        if b_in {
            let (_, _, p) = b.add_one(m_prereq, all, true, false)?;
            m_best_no_in = p & !m_prereq;
            if m_best_no_in == 0 {
                seen_zero = true;
                seen_zero_no_in = true;
            }
        }
        let mut m_prev = 0u64;
        loop {
            let mut m_next = all;
            for t in &terms {
                let m_this = t.prereq & !m_prereq;
                if m_this > m_prev && m_this < m_next {
                    m_next = m_this;
                }
            }
            m_prev = m_next;
            if m_next == all {
                break;
            }
            if m_next == m_best || m_next == m_best_no_in {
                continue;
            }
            let (uses_in, _, p) = b.add_one(m_prereq, m_next | m_prereq, ex_in, false)?;
            if p == m_prereq {
                seen_zero = true;
                if !uses_in {
                    seen_zero_no_in = true;
                }
            }
        }
        if !seen_zero {
            let (uses_in, _, _) = b.add_one(m_prereq, m_prereq, ex_in, false)?;
            if !uses_in {
                seen_zero_no_in = true;
            }
        }
        if !seen_zero_no_in {
            b.add_one(m_prereq, m_prereq, true, false)?;
        }
    }
    let loops = b.loops;
    Ok(Some(Arc::new(VtabCandidates {
        terms,
        loops,
        order_by,
    })))
}
