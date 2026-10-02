//! Response-time statistics over raw check samples: percentiles and the
//! bucketed series behind the latency charts.

use jiff::Timestamp;

/// One successful check's response time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub at: Timestamp,
    pub latency_ms: i64,
}

/// The nearest-rank percentile (`p` in `0.0..=100.0`) of an ascending slice.
pub fn percentile(sorted: &[i64], p: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let p = p.clamp(0.0, 100.0);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )] // slice lengths are tiny next to 2^52; the rank is within 1..=len
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

/// Percentiles over a set of samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub count: usize,
    pub p50: i64,
    pub p95: i64,
    pub max: i64,
}

pub fn summarize(samples: &[Sample]) -> Option<Summary> {
    let mut values: Vec<i64> = samples.iter().map(|s| s.latency_ms).collect();
    values.sort_unstable();
    Some(Summary {
        count: values.len(),
        p50: percentile(&values, 50.0)?,
        p95: percentile(&values, 95.0)?,
        max: *values.last()?,
    })
}

/// One slice of a chart's time axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bucket {
    pub start: Timestamp,
    /// `None` when no check landed in the slice.
    pub stats: Option<Summary>,
}

/// Splits `[from, to)` into `count` equal slices and summarises each.
///
/// Samples outside the range are ignored.
pub fn buckets(samples: &[Sample], from: Timestamp, to: Timestamp, count: usize) -> Vec<Bucket> {
    if count == 0 || to <= from {
        return Vec::new();
    }
    let span = to.as_millisecond() - from.as_millisecond();
    let width = (span / i64::try_from(count).unwrap_or(i64::MAX)).max(1);
    let mut groups: Vec<Vec<Sample>> = vec![Vec::new(); count];
    for sample in samples {
        if sample.at < from || sample.at >= to {
            continue;
        }
        let offset = sample.at.as_millisecond() - from.as_millisecond();
        let index = usize::try_from(offset / width).unwrap_or(0).min(count - 1);
        groups[index].push(*sample);
    }
    groups
        .iter()
        .enumerate()
        .map(|(i, group)| Bucket {
            start: Timestamp::from_millisecond(
                from.as_millisecond() + width * i64::try_from(i).unwrap_or(0),
            )
            .unwrap_or(from),
            stats: summarize(group),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    fn sample(secs: i64, latency_ms: i64) -> Sample {
        Sample {
            at: at(secs),
            latency_ms,
        }
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let values: Vec<i64> = (1..=100).collect();
        assert_eq!(percentile(&values, 50.0), Some(50));
        assert_eq!(percentile(&values, 95.0), Some(95));
        assert_eq!(percentile(&values, 100.0), Some(100));
        assert_eq!(percentile(&values, 0.0), Some(1));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn summary_reports_percentiles_and_max() {
        let samples: Vec<Sample> = (1..=20).map(|i| sample(i, i * 10)).collect();
        assert_eq!(
            summarize(&samples),
            Some(Summary {
                count: 20,
                p50: 100,
                p95: 190,
                max: 200
            })
        );
        assert_eq!(summarize(&[]), None);
    }

    #[test]
    fn buckets_split_the_range_and_leave_gaps_empty() {
        let samples = [sample(1, 10), sample(2, 30), sample(25, 50), sample(99, 7)];
        let out = buckets(&samples, at(0), at(100), 4);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].start, at(0));
        assert_eq!(out[0].stats.map(|s| (s.count, s.max)), Some((2, 30)));
        assert_eq!(out[1].stats.map(|s| s.p50), Some(50));
        assert_eq!(out[2].stats, None);
        assert_eq!(out[3].start, at(75));
        assert_eq!(out[3].stats.map(|s| s.p50), Some(7));
    }

    #[test]
    fn buckets_ignore_out_of_range_samples() {
        let out = buckets(&[sample(-5, 1), sample(100, 1)], at(0), at(100), 2);
        assert!(out.iter().all(|b| b.stats.is_none()));
        assert!(buckets(&[], at(10), at(10), 3).is_empty());
    }
}
