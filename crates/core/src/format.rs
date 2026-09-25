use chrono::{DateTime, Utc};

use crate::models::{MetricKind, MetricLine, MetricValue, Pace, PaceStatus, ProgressFormat};

/// A meter this close to its limit is treated as spent regardless of the clock.
const EXHAUSTED_REMAINING: f64 = 0.005;
/// Below this much of the window elapsed, a projection is too noisy to trust.
const MIN_ELAPSED_FRACTION: f64 = 0.08;
/// Projected end-of-window usage at or above these fractions of the limit.
const PROJECTED_RUN_OUT: f64 = 1.0;
const PROJECTED_CLOSE: f64 = 0.90;
/// Thresholds used when there is no usable window to project across.
const UNPACED_REMAINING: f64 = 0.10;
const UNPACED_RATIO: f64 = 0.80;

pub fn compact_number(n: f64) -> String {
    let abs = n.abs();
    if abs >= 1_000_000_000.0 {
        format!("{:.1}B", n / 1_000_000_000.0)
    } else if abs >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if abs >= 10_000.0 {
        format!("{:.1}K", n / 1_000.0)
    } else if abs >= 100.0 {
        format!("{:.0}", n)
    } else if abs >= 10.0 {
        format!("{:.1}", n)
    } else {
        format!("{:.2}", n)
    }
}

pub fn dollars(n: f64) -> String {
    if n.abs() >= 1000.0 {
        format!("${}", compact_number(n))
    } else {
        format!("${:.2}", n)
    }
}

pub fn percent(n: f64) -> String {
    format!("{:.0}%", n.round())
}

pub fn format_value(v: &MetricValue) -> String {
    match v.kind {
        MetricKind::Dollars => dollars(v.number),
        MetricKind::Percent => percent(v.number),
        MetricKind::Count => {
            let n = compact_number(v.number);
            match &v.label {
                Some(label) => format!("{n} {label}"),
                None => n,
            }
        }
    }
}

pub fn format_values(values: &[MetricValue]) -> String {
    values
        .iter()
        .map(format_value)
        .collect::<Vec<_>>()
        .join(" · ")
}

pub fn format_line(line: &MetricLine, used_mode: bool) -> String {
    match line {
        MetricLine::Progress {
            used,
            limit,
            format,
            ..
        } => match format {
            ProgressFormat::Percent => {
                let shown = if used_mode {
                    *used
                } else {
                    (100.0 - used).clamp(0.0, 100.0)
                };
                let word = if used_mode { "used" } else { "left" };
                format!("{} {word}", percent(shown))
            }
            ProgressFormat::Dollars => {
                if used_mode {
                    format!("{} of {}", dollars(*used), dollars(*limit))
                } else {
                    format!("{} left", dollars((limit - used).max(0.0)))
                }
            }
            ProgressFormat::Count { suffix } => {
                if used_mode {
                    format!("{:.0} / {:.0} {suffix}", used, limit)
                } else {
                    format!("{:.0} {suffix} left", (limit - used).max(0.0))
                }
            }
        },
        MetricLine::Values { values, .. } => format_values(values),
        MetricLine::Badge { text, .. } => text.clone(),
        MetricLine::Text { value, .. } => value.clone(),
        MetricLine::Chart { .. } => "trend".into(),
    }
}

pub fn used_ratio(line: &MetricLine) -> Option<f64> {
    match line {
        MetricLine::Progress { used, limit, .. } if *limit > 0.0 => {
            Some((*used / *limit).clamp(0.0, 1.0))
        }
        _ => None,
    }
}

/// Project a meter forward to its reset: blue on track, amber cutting it close, red running out.
///
/// Early in a window the sample is too small to extrapolate from, and some meters carry no
/// window at all, so both cases fall back to judging the level alone.
pub fn pace(
    used: f64,
    limit: f64,
    resets_at: Option<DateTime<Utc>>,
    period_ms: Option<i64>,
) -> Pace {
    let ratio = if limit > 0.0 { used / limit } else { 0.0 };
    let remaining = (1.0 - ratio).clamp(0.0, 1.0);
    let elapsed_fraction = elapsed_fraction(resets_at, period_ms);

    if remaining <= EXHAUSTED_REMAINING {
        return verdict(PaceStatus::Empty, 1.0, elapsed_fraction);
    }
    if let Some(elapsed) = elapsed_fraction.filter(|elapsed| *elapsed > MIN_ELAPSED_FRACTION) {
        let projected = ratio / elapsed;
        let status = if projected >= PROJECTED_RUN_OUT {
            PaceStatus::RunOut
        } else if projected >= PROJECTED_CLOSE {
            PaceStatus::Close
        } else {
            PaceStatus::OnTrack
        };
        return verdict(status, projected, elapsed_fraction);
    }
    let status = if remaining <= UNPACED_REMAINING {
        PaceStatus::Empty
    } else if ratio >= UNPACED_RATIO {
        PaceStatus::Close
    } else {
        PaceStatus::OnTrack
    };
    verdict(status, ratio, elapsed_fraction)
}

/// How far through its window a meter is, 0..1.
fn elapsed_fraction(resets_at: Option<DateTime<Utc>>, period_ms: Option<i64>) -> Option<f64> {
    let reset = resets_at?;
    let period = period_ms.filter(|period| *period > 0)? as f64;
    let left_ms = (reset - Utc::now()).num_milliseconds().max(0) as f64;
    Some(((period - left_ms) / period).clamp(0.0, 1.0))
}

fn verdict(status: PaceStatus, projected: f64, elapsed_fraction: Option<f64>) -> Pace {
    Pace {
        status,
        color: status.color().to_string(),
        projected,
        elapsed_fraction,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    /// A window of `period_ms` with `left_ms` still to run.
    fn window(period_ms: i64, left_ms: i64) -> (Option<DateTime<Utc>>, Option<i64>) {
        (
            Some(Utc::now() + Duration::milliseconds(left_ms)),
            Some(period_ms),
        )
    }

    #[test]
    fn projects_against_the_window_once_enough_of_it_has_elapsed() {
        // A quarter of the window gone, so usage is extrapolated by four.
        let (resets_at, period) = window(100_000, 75_000);
        for (used, expected) in [
            (10.0, PaceStatus::OnTrack),
            (23.0, PaceStatus::Close),
            (26.0, PaceStatus::RunOut),
            (50.0, PaceStatus::RunOut),
        ] {
            let verdict = pace(used, 100.0, resets_at, period);
            assert_eq!(verdict.status, expected, "{used} used");
            assert_eq!(verdict.color, expected.color());
            assert!(
                (verdict.projected - used / 25.0).abs() < 0.05,
                "{used} used"
            );
        }
    }

    #[test]
    fn an_exhausted_meter_is_spent_whatever_the_clock_says() {
        let (resets_at, period) = window(100_000, 99_000);
        let verdict = pace(99.6, 100.0, resets_at, period);
        assert_eq!(verdict.status, PaceStatus::Empty);
        assert_eq!(verdict.color, "#EF4444");
        assert_eq!(verdict.projected, 1.0);
    }

    #[test]
    fn too_little_of_the_window_elapsed_falls_back_to_the_level() {
        // Two percent in, a projection would be noise, so judge the level instead.
        let (resets_at, period) = window(100_000, 98_000);
        let verdict = pace(50.0, 100.0, resets_at, period);
        assert_eq!(verdict.status, PaceStatus::OnTrack);
        assert_eq!(verdict.projected, 0.5, "the level, not an extrapolation");
        assert!(verdict
            .elapsed_fraction
            .is_some_and(|elapsed| elapsed < 0.08));
    }

    #[test]
    fn a_meter_without_a_window_is_judged_on_its_level() {
        for (used, expected) in [
            (50.0, PaceStatus::OnTrack),
            (85.0, PaceStatus::Close),
            (95.0, PaceStatus::Empty),
        ] {
            let verdict = pace(used, 100.0, None, None);
            assert_eq!(verdict.status, expected, "{used} used");
            assert_eq!(verdict.elapsed_fraction, None);
        }
        // A reset time with no period, or a nonsense period, is not a usable window either.
        assert_eq!(
            pace(50.0, 100.0, Some(Utc::now()), None).elapsed_fraction,
            None
        );
        assert_eq!(
            pace(50.0, 100.0, Some(Utc::now()), Some(0)).elapsed_fraction,
            None
        );
    }

    #[test]
    fn an_overdue_reset_reads_as_a_finished_window() {
        let (resets_at, period) = window(100_000, -50_000);
        assert_eq!(
            pace(10.0, 100.0, resets_at, period).elapsed_fraction,
            Some(1.0)
        );
    }

    #[test]
    fn a_meter_without_a_limit_never_reads_as_spent() {
        let verdict = pace(42.0, 0.0, None, None);
        assert_eq!(verdict.status, PaceStatus::OnTrack);
        assert_eq!(verdict.projected, 0.0);
    }

    #[test]
    fn only_warning_states_are_worth_surfacing_unprompted() {
        assert!(PaceStatus::Close.is_warning());
        assert!(PaceStatus::RunOut.is_warning());
        assert!(!PaceStatus::OnTrack.is_warning());
        assert!(!PaceStatus::Empty.is_warning());
    }
}
