use std::{collections::BTreeMap, time::Duration};

use serde::{Deserialize, Serialize};

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct Histogram {
    count: u64,
    buckets_us: BTreeMap<u64, u64>,
}

impl Histogram {
    pub(crate) fn record(&mut self, value: Duration) {
        // Round up to a microsecond, then to a three-significant-digit upper bound.
        let micros = value.as_nanos().div_ceil(1_000) as u64;
        let step = bucket_step(micros);
        let upper_bound = micros.div_ceil(step).saturating_mul(step);
        *self.buckets_us.entry(upper_bound).or_default() += 1;
        self.count += 1;
    }

    pub(crate) fn percentile(&self, per_mille: u64) -> Option<u64> {
        let rank = (u128::from(self.count) * u128::from(per_mille)).div_ceil(1_000);
        let mut cumulative = 0_u128;
        self.buckets_us.iter().find_map(|(&upper_bound, &count)| {
            cumulative += u128::from(count);
            (cumulative >= rank).then_some(upper_bound)
        })
    }

    pub(crate) fn matches_sample_count(&self, expected: u64) -> bool {
        self.count == expected
            && self
                .buckets_us
                .iter()
                .all(|(&bound, &count)| count > 0 && bound.is_multiple_of(bucket_step(bound)))
            && self
                .buckets_us
                .values()
                .map(|&count| u128::from(count))
                .sum::<u128>()
                == u128::from(expected)
    }
}

fn bucket_step(micros: u64) -> u64 {
    10_u64.pow(micros.max(1).ilog10().saturating_sub(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_percentile_has_no_value() {
        assert_eq!(Histogram::default().percentile(999), None);
    }

    #[test]
    fn tail_percentile_uses_nearest_rank_and_upper_bounds() {
        let mut histogram = Histogram::default();
        for _ in 0..998 {
            histogram.record(Duration::from_nanos(999));
        }
        histogram.record(Duration::from_micros(12_345));
        histogram.record(Duration::from_micros(20_001));

        assert_eq!(histogram.percentile(990), Some(1));
        assert_eq!(histogram.percentile(999), Some(12_400));
        assert_eq!(histogram.percentile(1_000), Some(20_100));
    }
}
