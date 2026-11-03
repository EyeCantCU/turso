// median/percentile/percentile_cont/percentile_disc are built-ins
// (core/functions/percentile.rs).
use turso_ext::{AggFunc, AggregateDerive, ExtensionApi, Value};

pub fn register_extension(ext_api: &mut ExtensionApi) {
    unsafe {
        StandardDeviation::register_StandardDeviation(ext_api);
    }
}

/// Standard Deviation implementation using Welford's algorithm
/// Formula:
///
/// ```text
///     s = sqrt( M2 / (n - 1) )
/// ```
///
/// Where:
/// - `n` = number of observations
/// - `M2` = sum of squared deviations
#[derive(AggregateDerive)]
struct StandardDeviation;

impl AggFunc for StandardDeviation {
    type State = (u64, f64, f64); // Tracks the count, mean and sum of squared differences from the mean
    type Error = &'static str;
    const NAME: &'static str = "stddev";
    const ARGS: i32 = 1;

    fn step(state: &mut Self::State, args: &[Value]) {
        let (count, mean, m2) = state;

        if let Some(x) = args.first().and_then(Value::to_float) {
            *count += 1;

            // compute deviation from old mean
            let delta = x - *mean;
            *mean += delta / *count as f64;

            // update sum of squared differences
            let delta_2 = x - *mean;
            *m2 += delta * delta_2;
        }
    }

    fn finalize(state: Self::State) -> Result<Value, Self::Error> {
        let (count, _mean, m2) = state;
        if count < 2 {
            return Ok(Value::null());
        }

        let variance = m2 / (count - 1) as f64;
        Ok(Value::from_float(variance.sqrt()))
    }
}
