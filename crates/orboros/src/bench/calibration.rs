//! Calibration analysis for confidence vs benchmark outcome.
//!
//! For a saved run, looks at every result with a worker-reported
//! `confidence` value and asks: does higher confidence correspond to
//! higher pass rate? A well-calibrated worker reports 0.9 only when
//! it's about 90% likely to be correct.
//!
//! Output is a coarse bucketed histogram plus the Pearson correlation
//! between `confidence` and pass (1.0 / 0.0). The bucket histogram is
//! the thing a human actually reads; the correlation coefficient is
//! a single-number knob useful for `bench compare` over time.

use crate::bench::store::{BenchResult, BenchStatus};

/// Per-bucket calibration data over the `[0.0, 1.0]` confidence range.
/// Each bucket spans `bucket_width` and reports the number of results
/// that fell into it plus how many of those passed.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationBucket {
    /// Inclusive lower bound of the bucket.
    pub lo: f32,
    /// Exclusive upper bound of the bucket (except the last bucket,
    /// which is inclusive of 1.0).
    pub hi: f32,
    pub count: u32,
    pub passes: u32,
}

impl CalibrationBucket {
    /// Empirical pass rate within the bucket. Returns `None` when
    /// the bucket is empty (avoids fake 0/0 = 0.0 readings).
    #[must_use]
    pub fn pass_rate(&self) -> Option<f32> {
        if self.count == 0 {
            None
        } else {
            Some(f64::from(self.passes) as f32 / f64::from(self.count) as f32)
        }
    }
}

/// Calibration report for a single run.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationReport {
    /// All buckets in confidence order, even empty ones (lets the
    /// caller render a stable axis).
    pub buckets: Vec<CalibrationBucket>,
    /// Results with no confidence value are excluded from buckets;
    /// counted separately here so the operator knows what's missing.
    pub missing_confidence: u32,
    /// Pearson correlation between confidence (treated as `f32`) and
    /// pass (1.0 for `Pass`, else 0.0). `None` when fewer than two
    /// distinct samples are present.
    pub correlation: Option<f32>,
}

/// Computes the calibration report for a slice of results.
///
/// `bucket_count` must be ≥ 1; the function picks a sensible default
/// of 10 when given anything smaller.
#[must_use]
pub fn calibrate(results: &[BenchResult], bucket_count: usize) -> CalibrationReport {
    let n = bucket_count.max(1);
    let width = 1.0_f32 / n as f32;
    let mut buckets: Vec<CalibrationBucket> = (0..n)
        .map(|i| CalibrationBucket {
            lo: i as f32 * width,
            hi: (i + 1) as f32 * width,
            count: 0,
            passes: 0,
        })
        .collect();
    let mut missing = 0u32;
    let mut xs: Vec<f32> = Vec::new();
    let mut ys: Vec<f32> = Vec::new();

    for r in results {
        let Some(c) = r.confidence else {
            missing = missing.saturating_add(1);
            continue;
        };
        let pass = if r.status == BenchStatus::Pass {
            1.0
        } else {
            0.0
        };
        xs.push(c);
        ys.push(pass);
        let mut idx = (c / width).floor() as isize;
        if idx >= n as isize {
            // c == 1.0 lands in the last bucket inclusively.
            idx = n as isize - 1;
        } else if idx < 0 {
            idx = 0;
        }
        let b = &mut buckets[idx as usize];
        b.count = b.count.saturating_add(1);
        if pass > 0.5 {
            b.passes = b.passes.saturating_add(1);
        }
    }

    CalibrationReport {
        buckets,
        missing_confidence: missing,
        correlation: pearson_correlation(&xs, &ys),
    }
}

/// Renders a calibration report as a short human-readable summary.
/// Intended for the `bench calibration <run_id>` CLI handler.
#[must_use]
pub fn render_report(report: &CalibrationReport) -> String {
    let mut out = String::new();
    out.push_str("confidence bucket    count   passes   pass rate\n");
    for b in &report.buckets {
        let rate = b
            .pass_rate()
            .map_or(String::from("    —"), |r| format!("{:.2}", r));
        out.push_str(&format!(
            "[{lo:.2}, {hi:.2})    {count:>4}   {passes:>4}     {rate}\n",
            lo = b.lo,
            hi = b.hi,
            count = b.count,
            passes = b.passes,
        ));
    }
    out.push_str(&format!(
        "\nresults without confidence: {}\n",
        report.missing_confidence,
    ));
    match report.correlation {
        Some(c) => out.push_str(&format!(
            "correlation(confidence, pass): {c:+.3}  (1.0 = perfectly calibrated)\n",
        )),
        None => out.push_str("correlation: insufficient samples\n"),
    }
    out
}

fn pearson_correlation(xs: &[f32], ys: &[f32]) -> Option<f32> {
    let n = xs.len();
    if n < 2 || n != ys.len() {
        return None;
    }
    let mean_x = xs.iter().sum::<f32>() / n as f32;
    let mean_y = ys.iter().sum::<f32>() / n as f32;
    let mut cov = 0.0_f32;
    let mut var_x = 0.0_f32;
    let mut var_y = 0.0_f32;
    for (x, y) in xs.iter().zip(ys.iter()) {
        let dx = *x - mean_x;
        let dy = *y - mean_y;
        cov += dx * dy;
        var_x += dx * dx;
        var_y += dy * dy;
    }
    let denom = (var_x * var_y).sqrt();
    if denom.abs() < f32::EPSILON {
        None
    } else {
        Some(cov / denom)
    }
}

/// CLI handler: reads results for `run_id` and prints the calibration
/// report.
///
/// # Errors
///
/// Returns an error if the store can't be read or the run is unknown.
pub fn cmd_bench_calibration(
    store: &crate::bench::store::BenchStore,
    run_id: &str,
    bucket_count: usize,
) -> anyhow::Result<()> {
    let results = store.read_results(run_id)?;
    if results.is_empty() {
        anyhow::bail!("no results found for run `{run_id}`");
    }
    let report = calibrate(&results, bucket_count);
    print!("{}", render_report(&report));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::case::BenchTier;

    fn r(case_id: &str, confidence: Option<f32>, status: BenchStatus) -> BenchResult {
        BenchResult {
            case_id: case_id.into(),
            run_id: "run".into(),
            tier: BenchTier::T1,
            status,
            score: if status == BenchStatus::Pass {
                1.0
            } else {
                0.0
            },
            latency_ms: 0,
            cost_cents: 0,
            iterations: 1,
            worker_model: "m".into(),
            prompt_hash: "h".into(),
            system_prompt_hash: None,
            system_prompt_source: None,
            confidence,
            error: None,
        }
    }

    // ── bucket bookkeeping ────────────────────────────────────

    #[test]
    fn empty_input_produces_empty_buckets() {
        let report = calibrate(&[], 10);
        assert_eq!(report.buckets.len(), 10);
        assert_eq!(report.missing_confidence, 0);
        assert!(report.buckets.iter().all(|b| b.count == 0));
        assert!(report.correlation.is_none());
    }

    #[test]
    fn missing_confidence_results_are_counted_separately() {
        let res = vec![
            r("a", None, BenchStatus::Pass),
            r("b", None, BenchStatus::Fail),
            r("c", Some(0.5), BenchStatus::Pass),
        ];
        let report = calibrate(&res, 10);
        assert_eq!(report.missing_confidence, 2);
        // Only the one with-confidence result lands in a bucket.
        let total: u32 = report.buckets.iter().map(|b| b.count).sum();
        assert_eq!(total, 1);
    }

    #[test]
    fn confidence_of_one_lands_in_last_bucket() {
        let report = calibrate(&[r("a", Some(1.0), BenchStatus::Pass)], 10);
        assert_eq!(report.buckets.last().unwrap().count, 1);
    }

    #[test]
    fn confidence_of_zero_lands_in_first_bucket() {
        let report = calibrate(&[r("a", Some(0.0), BenchStatus::Fail)], 10);
        assert_eq!(report.buckets[0].count, 1);
        assert_eq!(report.buckets[0].passes, 0);
    }

    #[test]
    fn bucket_pass_rate_is_none_for_empty_bucket() {
        let report = calibrate(&[r("a", Some(0.5), BenchStatus::Pass)], 10);
        let empty = report.buckets.iter().find(|b| b.count == 0).unwrap();
        assert!(empty.pass_rate().is_none());
    }

    // ── correlation ──────────────────────────────────────────

    #[test]
    fn correlation_is_positive_when_confidence_tracks_pass() {
        let mut res = Vec::new();
        for i in 0..10 {
            let c = i as f32 / 9.0;
            let status = if i >= 5 {
                BenchStatus::Pass
            } else {
                BenchStatus::Fail
            };
            res.push(r(&format!("c{i}"), Some(c), status));
        }
        let report = calibrate(&res, 10);
        let corr = report.correlation.unwrap();
        assert!(
            corr > 0.7,
            "expected strong positive correlation, got {corr}"
        );
    }

    #[test]
    fn correlation_is_negative_when_confidence_anti_correlates() {
        let mut res = Vec::new();
        for i in 0..10 {
            let c = i as f32 / 9.0;
            // Confident answers all fail; doubtful ones pass.
            let status = if i >= 5 {
                BenchStatus::Fail
            } else {
                BenchStatus::Pass
            };
            res.push(r(&format!("c{i}"), Some(c), status));
        }
        let report = calibrate(&res, 10);
        let corr = report.correlation.unwrap();
        assert!(
            corr < -0.7,
            "expected strong negative correlation, got {corr}"
        );
    }

    #[test]
    fn correlation_is_none_with_one_sample() {
        let report = calibrate(&[r("a", Some(0.5), BenchStatus::Pass)], 10);
        assert!(report.correlation.is_none());
    }

    #[test]
    fn correlation_is_none_when_outcomes_are_uniform() {
        // All passes, all same confidence → zero variance.
        let res = vec![
            r("a", Some(0.5), BenchStatus::Pass),
            r("b", Some(0.5), BenchStatus::Pass),
            r("c", Some(0.5), BenchStatus::Pass),
        ];
        let report = calibrate(&res, 10);
        assert!(report.correlation.is_none());
    }

    // ── rendering ────────────────────────────────────────────

    #[test]
    fn render_report_includes_buckets_and_correlation_label() {
        let res = vec![
            r("a", Some(0.9), BenchStatus::Pass),
            r("b", Some(0.1), BenchStatus::Fail),
        ];
        let report = calibrate(&res, 10);
        let s = render_report(&report);
        assert!(s.contains("confidence bucket"));
        assert!(s.contains("correlation"));
        assert!(s.contains("results without confidence: 0"));
    }

    #[test]
    fn render_report_handles_insufficient_samples_message() {
        let res = vec![r("a", Some(0.5), BenchStatus::Pass)];
        let report = calibrate(&res, 10);
        let s = render_report(&report);
        assert!(s.contains("insufficient samples"));
    }

    // ── bucket_count guard ───────────────────────────────────

    #[test]
    fn zero_buckets_clamps_to_one() {
        let res = vec![r("a", Some(0.5), BenchStatus::Pass)];
        let report = calibrate(&res, 0);
        assert_eq!(report.buckets.len(), 1);
        assert_eq!(report.buckets[0].count, 1);
    }
}
