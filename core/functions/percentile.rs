//! median(), percentile(), percentile_cont() and percentile_disc() as built
//! into SQLite 3.54 with SQLITE_ENABLE_PERCENTILE (func.c percentStep and
//! friends). Payload: `[fraction (Null until the first row), y values...]`.

use crate::alloc::TursoVecExt;
use crate::function::AggFunc;
use crate::numeric::Numeric;
use crate::types::Value;
use crate::vdbe::affinity::{try_for_float, NumericParseResult, ParsedNumber};
use crate::{LimboError, Result};

pub fn is_percentile(func: &AggFunc) -> bool {
    matches!(
        func,
        AggFunc::Median
            | AggFunc::Percentile
            | AggFunc::PercentileContCall
            | AggFunc::PercentileDiscCall
    )
}

fn err(msg: String) -> LimboError {
    LimboError::InvalidArgument(msg)
}

/// sqlite3_value_numeric_type() + sqlite3_value_double(): None unless the
/// value is (or converts entirely to) an INTEGER or REAL.
fn numeric_value(v: &Value) -> Option<f64> {
    match v {
        Value::Numeric(Numeric::Integer(i)) => Some(*i as f64),
        Value::Numeric(Numeric::Float(f)) => Some(f64::from(*f)),
        Value::Text(t) => match try_for_float(t.as_str().as_bytes()) {
            (NumericParseResult::PureInteger | NumericParseResult::HasDecimalOrExp, n) => match n {
                ParsedNumber::Integer(i) => Some(i as f64),
                ParsedNumber::Float(f) => Some(f),
                ParsedNumber::None => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn float_of(v: &Value) -> Option<f64> {
    match v {
        Value::Numeric(Numeric::Float(f)) => Some(f64::from(*f)),
        _ => None,
    }
}

pub fn init(payload: &mut crate::alloc::Vec<Value>) -> Result<()> {
    payload.try_reserve_exact(1)?;
    payload.push(Value::Null);
    Ok(())
}

/// xStep: validate the fraction on every row, then remember a non-NULL Y.
pub fn step(
    func: &AggFunc,
    y: &Value,
    fraction: Option<&Value>,
    payload: &mut crate::alloc::Vec<Value>,
) -> Result<()> {
    let name = func.as_str();
    let pct = if matches!(func, AggFunc::Median) {
        0.5
    } else {
        let mx = if matches!(func, AggFunc::Percentile) {
            100.0
        } else {
            1.0
        };
        match fraction.and_then(numeric_value).map(|p| p / mx) {
            Some(p) if (0.0..=1.0).contains(&p) => p,
            _ => {
                return Err(err(format!(
                    "the fraction argument to {name}() is not between 0.0 and {mx:.1}"
                )))
            }
        }
    };
    match float_of(&payload[0]) {
        None => payload[0] = Value::from_f64(pct),
        Some(prev) => {
            let d = prev - pct;
            if !(-0.001..=0.001).contains(&d) {
                return Err(err(format!(
                    "the fraction argument to {name}() is not the same for all input rows"
                )));
            }
        }
    }
    let y = match y {
        Value::Null => return Ok(()),
        Value::Numeric(Numeric::Integer(i)) => *i as f64,
        Value::Numeric(Numeric::Float(f)) => f64::from(*f),
        _ => return Err(err(format!("input to {name}() is not numeric"))),
    };
    if y.is_infinite() {
        return Err(err(format!("Inf input to {name}()")));
    }
    payload.try_push(Value::from_f64(y))?;
    Ok(())
}

/// xInverse: forget one Y equal to the departing row's Y.
pub fn inverse(y: &Value, payload: &mut crate::alloc::Vec<Value>) {
    let y = match y {
        Value::Numeric(Numeric::Integer(i)) => *i as f64,
        Value::Numeric(Numeric::Float(f)) => f64::from(*f),
        _ => return,
    };
    if y.is_infinite() {
        return;
    }
    if let Some(pos) = payload[1..].iter().position(|v| float_of(v) == Some(y)) {
        payload.remove(pos + 1);
    }
}

/// xFinal / xValue.
pub fn finalize(func: &AggFunc, payload: &[Value]) -> Value {
    let Some(pct) = float_of(&payload[0]) else {
        return Value::Null;
    };
    let mut a: Vec<f64> = payload[1..].iter().filter_map(float_of).collect();
    if a.is_empty() {
        return Value::Null;
    }
    a.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let n = a.len();
    let ix = pct * (n - 1) as f64;
    let i1 = (ix as u32) as usize;
    let i1 = i1.min(n - 1);
    let vx = if matches!(func, AggFunc::PercentileDiscCall) {
        a[i1]
    } else {
        let i2 = if ix == i1 as f64 || i1 == n - 1 {
            i1
        } else {
            i1 + 1
        };
        let (v1, v2) = (a[i1], a[i2]);
        v1 + (v2 - v1) * (ix - i1 as f64)
    };
    Value::from_f64(vx)
}
