//! The planner's side of SQLite's `sqlite3_index_info`: what one xBestIndex
//! call sees and what it returns.

use crate::{LimboError, Value};
use turso_ext::{
    ConstraintInfo, ConstraintOp, ConstraintUsage, IndexInfo, OrderByInfo, ResultCode,
};

/// `SQLITE_INDEX_CONSTRAINT_*` codes.
pub mod index_op {
    pub const EQ: u8 = 2;
    pub const GT: u8 = 4;
    pub const LE: u8 = 8;
    pub const LT: u8 = 16;
    pub const GE: u8 = 32;
    pub const MATCH: u8 = 64;
    pub const LIKE: u8 = 65;
    pub const GLOB: u8 = 66;
    pub const REGEXP: u8 = 67;
    pub const NE: u8 = 68;
    pub const ISNOT: u8 = 69;
    pub const ISNOTNULL: u8 = 70;
    pub const ISNULL: u8 = 71;
    pub const IS: u8 = 72;
    pub const LIMIT: u8 = 73;
    pub const OFFSET: u8 = 74;
    pub const FUNCTION: u8 = 150;
}

/// `SQLITE_INDEX_SCAN_UNIQUE`.
pub const INDEX_SCAN_UNIQUE: i32 = 1;
/// `SQLITE_INDEX_SCAN_HEX`.
pub const INDEX_SCAN_HEX: i32 = 2;

/// One `aConstraint` entry plus what the `sqlite3_vtab_*` helpers report for it.
#[derive(Debug, Clone)]
pub struct IndexConstraint {
    /// Column number; -1 for the rowid.
    pub column: i32,
    /// An [`index_op`] code.
    pub op: u8,
    pub usable: bool,
    /// `sqlite3_vtab_collation`.
    pub collation: String,
    /// `sqlite3_vtab_rhs_value`: the right-hand side when it is a literal.
    pub rhs: Option<Value>,
    /// `sqlite3_vtab_in`: the constraint is an `IN (...)`.
    pub is_in: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexOrderBy {
    /// Column number; -1 for the rowid.
    pub column: i32,
    pub desc: bool,
}

/// The input of one xBestIndex call.
#[derive(Debug, Clone, Default)]
pub struct IndexRequest {
    pub constraints: Vec<IndexConstraint>,
    pub order_by: Vec<IndexOrderBy>,
    /// `colUsed`.
    pub col_used: u64,
    /// `sqlite3_vtab_distinct`: 0 to 3.
    pub distinct: i32,
}

/// The output of one xBestIndex call.
#[derive(Debug, Clone)]
pub struct IndexPlan {
    /// One entry per request constraint.
    pub usages: Vec<ConstraintUsage>,
    /// Per constraint: an IN handled all at once (`sqlite3_vtab_in(.., 1)`).
    pub in_all_at_once: Vec<bool>,
    pub idx_num: i32,
    /// Opaque to the planner; handed back to the cursor's filter.
    pub idx_str: Option<String>,
    /// idxStr as text, for EXPLAIN QUERY PLAN.
    pub idx_str_display: Option<String>,
    pub order_by_consumed: bool,
    pub estimated_cost: f64,
    /// Negative when the module gave no estimate.
    pub estimated_rows: i64,
    /// `SQLITE_INDEX_SCAN_*` flags.
    pub idx_flags: i32,
    /// Built by the older `best_index` interface, which has no cost model.
    pub legacy: bool,
}

impl IndexPlan {
    /// A plan that uses no constraint, with SQLite's defaults.
    pub fn new(n_constraints: usize) -> Self {
        Self {
            usages: vec![
                ConstraintUsage {
                    argv_index: None,
                    omit: false,
                };
                n_constraints
            ],
            in_all_at_once: vec![false; n_constraints],
            idx_num: 0,
            idx_str: None,
            idx_str_display: None,
            order_by_consumed: false,
            estimated_cost: 1e99 / 2.0,
            estimated_rows: 25,
            idx_flags: 0,
            legacy: false,
        }
    }
}

fn legacy_op(c: &IndexConstraint) -> Option<ConstraintOp> {
    if c.column < 0 || c.is_in {
        return None;
    }
    Some(match c.op {
        index_op::EQ => ConstraintOp::Eq,
        index_op::LT => ConstraintOp::Lt,
        index_op::LE => ConstraintOp::Le,
        index_op::GT => ConstraintOp::Gt,
        index_op::GE => ConstraintOp::Ge,
        index_op::NE => ConstraintOp::Ne,
        _ => return None,
    })
}

/// Runs a request through the older `best_index(constraints, order_by)`
/// interface. Only the constraints that interface always saw are passed.
/// `Ok(None)` means SQLITE_CONSTRAINT: this set of usable constraints gives no plan.
pub(crate) fn best_index_via_legacy(
    req: &IndexRequest,
    best_index: impl FnOnce(&[ConstraintInfo], &[OrderByInfo]) -> Result<IndexInfo, ResultCode>,
) -> crate::Result<Option<IndexPlan>> {
    let mut positions = Vec::new();
    let constraints: Vec<ConstraintInfo> = req
        .constraints
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            legacy_op(c).map(|op| {
                positions.push(i);
                ConstraintInfo {
                    column_index: c.column as u32,
                    op,
                    usable: c.usable,
                    index: i,
                }
            })
        })
        .collect();
    let order_by: Vec<OrderByInfo> = if req.order_by.iter().all(|o| o.column >= 0) {
        req.order_by
            .iter()
            .map(|o| OrderByInfo {
                column_index: o.column as u32,
                desc: o.desc,
            })
            .collect()
    } else {
        Vec::new()
    };
    let info = match best_index(&constraints, &order_by) {
        Ok(info) => info,
        Err(ResultCode::ConstraintViolation) => return Ok(None),
        Err(rc) => return Err(LimboError::from(rc)),
    };
    if info.constraint_usages.len() != constraints.len() {
        return Err(LimboError::ExtensionError(format!(
            "Constraint usage count mismatch (expected {}, got {})",
            constraints.len(),
            info.constraint_usages.len()
        )));
    }
    let mut plan = IndexPlan::new(req.constraints.len());
    for (usage, &pos) in info.constraint_usages.iter().zip(&positions) {
        plan.usages[pos] = *usage;
    }
    plan.idx_num = info.idx_num;
    plan.idx_str_display = info.idx_str.clone();
    plan.idx_str = info.idx_str;
    plan.order_by_consumed = info.order_by_consumed && !order_by.is_empty();
    plan.estimated_cost = info.estimated_cost;
    plan.estimated_rows = if info.estimated_rows == u32::MAX {
        -1
    } else {
        i64::from(info.estimated_rows)
    };
    plan.legacy = true;
    Ok(Some(plan))
}
