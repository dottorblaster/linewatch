//! Dossier — deterministic pure projection of the event log.
//!
//! [`project`] verifies the hash chain, parses every record, and computes
//! a [`Dossier`] with outage statistics, downtime bands, temperature
//! correlation, and event-level indemnity estimates.

use std::time::Duration;

use serde_json::Value;
use time::{OffsetDateTime, Time};

use crate::core::chain::{ChainStatus, Record, verify_chain};
use crate::core::events::{AgcomCategory, OutageEvent};

// ---------------------------------------------------------------------------
// Input models
// ---------------------------------------------------------------------------

/// Service-level commitments that govern indemnity accrual.
#[derive(Debug, Clone)]
pub struct ServiceCharter {
    /// Number of full days before indemnity starts accruing.
    pub repair_window_days: u32,
}

/// Daily tariff amounts.
#[derive(Debug, Clone)]
pub struct Tariff {
    /// Indemnity per full day of CompleteInterruption.
    pub complete_per_day: f64,
    /// Indemnity per full day of IrregularService.
    pub irregular_per_day: f64,
}

/// Configuration for temperature-correlation analysis.
#[derive(Debug, Clone)]
pub struct DossierConfig {
    /// Hour when the daytime window starts (0–23).
    pub daytime_start: u8,
    /// Hour when the daytime window ends (exclusive, 0–23).
    pub daytime_end: u8,
    /// Temperature threshold in °C.
    pub temp_threshold: f64,
}

// ---------------------------------------------------------------------------
// Output models
// ---------------------------------------------------------------------------

/// Temperature correlation summary.
#[derive(Debug, Clone, PartialEq)]
pub struct TempCorrelation {
    /// Total downtime in the configured daytime window (seconds).
    pub downtime_in_window: Duration,
    /// Portion of `downtime_in_window` where temperature exceeded the
    /// threshold (seconds).
    pub downtime_above_threshold: Duration,
    /// Ratio `downtime_above_threshold / downtime_in_window` (0.0–1.0).
    pub share_above_threshold: f64,
}

/// One line of explainable indemnity for a single outage event.
#[derive(Debug, Clone, PartialEq)]
pub struct IndemnityLine {
    pub event_start: String,
    pub event_end: String,
    pub duration_days: f64,
    pub category: String,
    pub repair_window_days: u32,
    pub tariff_daily: f64,
    /// Human-readable formula, e.g.
    /// `"max(0, 2.50 - 1) × 100.00 = 150.00"`.
    pub formula: String,
    pub indemnity: f64,
}

/// Hour-band downtime: [0–6, 6–12, 12–18, 18–24).
pub type HourBands = [Duration; 4];

/// The complete dossier produced by [`project`].
#[derive(Debug, Clone, PartialEq)]
pub struct Dossier {
    pub chain_status: ChainStatus,
    pub days_observed: f64,
    pub outage_count: u32,
    pub total_downtime: Duration,
    pub downtime_by_hour_band: HourBands,
    pub temp_correlation: Option<TempCorrelation>,
    pub per_event_indemnities: Vec<IndemnityLine>,
}

// ---------------------------------------------------------------------------
// Pure projection
// ---------------------------------------------------------------------------

/// Project a dossier from raw JSONL lines, a charter, a tariff, and an
/// analysis config.
///
/// 1. Verifies the hash chain via [`verify_chain`].
/// 2. Parses every line into a [`Record`].
/// 3. Computes all statistics purely from the parsed records.
///
/// When the chain is broken the affected range is marked `compromised` in
/// the returned [`ChainStatus`] and no integrity assertions are made for
/// those records.
pub fn project(
    lines: &[Value],
    charter: &ServiceCharter,
    tariff: &Tariff,
    cfg: &DossierConfig,
) -> Dossier {
    let chain_status = verify_chain(lines);

    // Parse all lines into Records.
    let records: Vec<Record> = lines
        .iter()
        .filter_map(|v| serde_json::from_value::<Record>(v.clone()).ok())
        .collect();

    // Collect outage events (only those with an end timestamp).
    let outages: Vec<&OutageEvent> = records
        .iter()
        .filter_map(|r| match r {
            Record::Outage { event, .. } => Some(event),
            _ => None,
        })
        .filter(|e| e.ended.is_some())
        .collect();

    // --- Days observed ---
    let days_observed = compute_days_observed(&records);

    // --- Outage count & total downtime ---
    let outage_count = outages.len() as u32;
    let total_downtime: Duration = outages
        .iter()
        .filter_map(|e| {
            let end = e.ended?;
            let dur = end - e.started;
            Some(Duration::from_secs_f64(dur.as_seconds_f64().max(0.0)))
        })
        .sum();

    // --- Downtime by hour band ---
    let downtime_by_hour_band = compute_hour_bands(&outages);

    // --- Temperature correlation ---
    let temp_correlation = compute_temp_correlation(&records, &outages, cfg);

    // --- Per-event indemnities ---
    let per_event_indemnities: Vec<IndemnityLine> = outages
        .iter()
        .filter_map(|e| compute_indemnity(e, charter, tariff))
        .collect();

    Dossier {
        chain_status,
        days_observed,
        outage_count,
        total_downtime,
        downtime_by_hour_band,
        temp_correlation,
        per_event_indemnities,
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn compute_days_observed(records: &[Record]) -> f64 {
    let mut first_ts: Option<OffsetDateTime> = None;
    let mut last_ts: Option<OffsetDateTime> = None;

    for r in records {
        let ts = match r {
            Record::Sample { ts, .. } => {
                OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339).ok()
            }
            Record::Outage { event, .. } => Some(event.started),
            Record::MonitorRestart { .. } => None,
        };
        if let Some(t) = ts {
            if first_ts.is_none() || t < first_ts.unwrap() {
                first_ts = Some(t);
            }
            if last_ts.is_none() || t > last_ts.unwrap() {
                last_ts = Some(t);
            }
        }
    }

    match (first_ts, last_ts) {
        (Some(first), Some(last)) => {
            let span = last - first;
            span.as_seconds_f64() / 86400.0
        }
        _ => 0.0,
    }
}

fn compute_hour_bands(outages: &[&OutageEvent]) -> HourBands {
    let mut bands: HourBands = [Duration::ZERO; 4];

    for event in outages {
        let end = match event.ended {
            Some(e) => e,
            None => continue,
        };
        let mut cursor = event.started;
        while cursor < end {
            let band_end = if cursor.date() == end.date() && cursor.hour() == end.hour() {
                end
            } else {
                let next_hour = cursor.replace_time(
                    Time::from_hms((cursor.hour() + 1) % 24, 0, 0).expect("valid hour"),
                );
                if next_hour > end { end } else { next_hour }
            };
            let segment = (band_end - cursor).as_seconds_f64().max(0.0);
            let idx = (cursor.hour() / 6) as usize;
            bands[idx] += Duration::from_secs_f64(segment);
            cursor = band_end;
        }
    }

    bands
}

fn compute_temp_correlation(
    records: &[Record],
    outages: &[&OutageEvent],
    cfg: &DossierConfig,
) -> Option<TempCorrelation> {
    // Collect all timed samples with temperature.
    let temp_samples: Vec<(OffsetDateTime, f64)> = records
        .iter()
        .filter_map(|r| match r {
            Record::Sample { ts, temp_c, .. } => {
                let t = OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339)
                    .ok()?;
                Some((t, (*temp_c)?))
            }
            _ => None,
        })
        .collect();

    if temp_samples.is_empty() {
        return None;
    }

    let daytime_start = cfg.daytime_start;
    let daytime_end = cfg.daytime_end;

    // Total downtime in the daytime window.
    let mut downtime_in_window = Duration::ZERO;
    // Downtime in the daytime window AND above threshold.
    let mut downtime_above_threshold = Duration::ZERO;

    for event in outages {
        let end = match event.ended {
            Some(e) => e,
            None => continue,
        };
        let mut cursor = event.started;
        while cursor < end {
            let hour = cursor.hour();
            let in_window = hour >= daytime_start && hour < daytime_end;

            let band_end = if cursor.date() == end.date() && cursor.hour() == end.hour() {
                end
            } else {
                let next_hour = cursor.replace_time(
                    Time::from_hms((cursor.hour() + 1) % 24, 0, 0).expect("valid hour"),
                );
                if next_hour > end { end } else { next_hour }
            };
            let segment_dur =
                Duration::from_secs_f64((band_end - cursor).as_seconds_f64().max(0.0));

            if in_window {
                downtime_in_window += segment_dur;
                // Use the nearest sample at-or-before the start of this
                // segment as the temperature for the entire segment.
                let temp = temp_samples
                    .iter()
                    .rev()
                    .find(|(t, _)| *t <= cursor)
                    .or_else(|| {
                        // Fallback: next sample after cursor.
                        temp_samples.iter().find(|(t, _)| *t >= cursor)
                    })
                    .map(|(_, temp)| *temp);

                if let Some(t) = temp
                    && t > cfg.temp_threshold
                {
                    downtime_above_threshold += segment_dur;
                }
            }
            cursor = band_end;
        }
    }

    if downtime_in_window.is_zero() {
        return None;
    }

    let share = downtime_above_threshold.as_secs_f64() / downtime_in_window.as_secs_f64();

    Some(TempCorrelation {
        downtime_in_window,
        downtime_above_threshold,
        share_above_threshold: share,
    })
}

fn compute_indemnity(
    event: &OutageEvent,
    charter: &ServiceCharter,
    tariff: &Tariff,
) -> Option<IndemnityLine> {
    let end = event.ended?;
    let duration_secs = (end - event.started).as_seconds_f64().max(0.0);
    let duration_days = duration_secs / 86400.0;

    let (category_str, daily_rate) = match event.category {
        AgcomCategory::CompleteInterruption => ("complete_interruption", tariff.complete_per_day),
        AgcomCategory::IrregularService => ("irregular_service", tariff.irregular_per_day),
    };

    let effective_days = (duration_days - charter.repair_window_days as f64).max(0.0);
    let indemnity = effective_days * daily_rate;

    let formula = format!(
        "max(0, {:.4} - {}) × {:.2} = {:.2}",
        duration_days, charter.repair_window_days, daily_rate, indemnity,
    );

    Some(IndemnityLine {
        event_start: event.started.to_string(),
        event_end: end.to_string(),
        duration_days,
        category: category_str.to_string(),
        repair_window_days: charter.repair_window_days,
        tariff_daily: daily_rate,
        formula,
        indemnity,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::chain::{Record, RecordChain, compute_hash};
    use crate::core::events::OutageEvent;
    use crate::core::types::Status;
    use serde_json::json;
    use std::time::Duration as StdDuration;
    use time::macros::datetime;

    // ------------------------------------------------------------------
    // Fixture helpers
    // ------------------------------------------------------------------

    fn mk_sample_record(seq: u64, prev_hash: &str, ts: &str, temp_c: Option<f64>) -> Value {
        let body = Record::Sample {
            chain: RecordChain {
                seq,
                prev_hash: prev_hash.to_owned(),
                hash: String::new(),
            },
            ts: ts.to_owned(),
            temp_c,
            outcomes: vec![],
        };
        let hash = compute_hash(&body, prev_hash);
        let body = Record::Sample {
            chain: RecordChain {
                seq,
                prev_hash: prev_hash.to_owned(),
                hash,
            },
            ts: ts.to_owned(),
            temp_c,
            outcomes: vec![],
        };
        serde_json::to_value(&body).unwrap()
    }

    fn mk_outage_record(
        seq: u64,
        prev_hash: &str,
        started: OffsetDateTime,
        ended: OffsetDateTime,
        category: AgcomCategory,
        temp_c: Option<f64>,
    ) -> Value {
        let event = OutageEvent {
            started,
            ended: Some(ended),
            worst_status: match &category {
                AgcomCategory::CompleteInterruption => Status::Down,
                AgcomCategory::IrregularService => Status::Degraded,
            },
            min_temp_c: temp_c,
            samples_count: 0,
            category: category.clone(),
        };
        let body = Record::Outage {
            chain: RecordChain {
                seq,
                prev_hash: prev_hash.to_owned(),
                hash: String::new(),
            },
            event,
            hops: vec![],
        };
        let hash = compute_hash(&body, prev_hash);
        let body = Record::Outage {
            chain: RecordChain {
                seq,
                prev_hash: prev_hash.to_owned(),
                hash,
            },
            event: OutageEvent {
                started,
                ended: Some(ended),
                worst_status: match category {
                    AgcomCategory::CompleteInterruption => Status::Down,
                    AgcomCategory::IrregularService => Status::Degraded,
                },
                min_temp_c: temp_c,
                samples_count: 0,
                category,
            },
            hops: vec![],
        };
        serde_json::to_value(&body).unwrap()
    }

    fn mk_restart_record(seq: u64, prev_hash: &str) -> Value {
        let body = Record::MonitorRestart {
            chain: RecordChain {
                seq,
                prev_hash: prev_hash.to_owned(),
                hash: String::new(),
            },
        };
        let hash = compute_hash(&body, prev_hash);
        let body = Record::MonitorRestart {
            chain: RecordChain {
                seq,
                prev_hash: prev_hash.to_owned(),
                hash,
            },
        };
        serde_json::to_value(&body).unwrap()
    }

    fn build_fixture() -> Vec<Value> {
        // A 3-day log with:
        //   - 2 CompleteInterruption outages
        //   - 1 IrregularService outage
        //   - periodic samples with known temperatures
        let mut lines = Vec::new();

        let mut prev = String::new();
        let mut seq = 0u64;

        // Day 0 — restart + samples
        lines.push(mk_restart_record(seq, &prev));
        prev = lines.last().unwrap()["hash"].as_str().unwrap().to_owned();
        seq += 1;

        // Samples on day 0
        for h in &[6, 8, 10, 12, 14, 16] {
            lines.push(mk_sample_record(
                seq,
                &prev,
                &format!("2026-07-10T{:02}:00:00Z", h),
                Some(if *h >= 12 { 36.0 } else { 22.0 }),
            ));
            prev = lines.last().unwrap()["hash"].as_str().unwrap().to_owned();
            seq += 1;
        }

        // Outage 1: CompleteInterruption, 6 hours, starts during daytime
        let o1_start = datetime!(2026-07-10 13:00:00 UTC);
        let o1_end = datetime!(2026-07-10 19:00:00 UTC);
        lines.push(mk_outage_record(
            seq,
            &prev,
            o1_start,
            o1_end,
            AgcomCategory::CompleteInterruption,
            Some(36.0),
        ));
        prev = lines.last().unwrap()["hash"].as_str().unwrap().to_owned();
        seq += 1;

        // Samples day 1
        for h in &[6, 8, 10, 12, 14, 16] {
            lines.push(mk_sample_record(
                seq,
                &prev,
                &format!("2026-07-11T{:02}:00:00Z", h),
                Some(if *h >= 12 { 38.0 } else { 20.0 }),
            ));
            prev = lines.last().unwrap()["hash"].as_str().unwrap().to_owned();
            seq += 1;
        }

        // Outage 2: IrregularService, 2 hours
        let o2_start = datetime!(2026-07-11 08:00:00 UTC);
        let o2_end = datetime!(2026-07-11 10:00:00 UTC);
        lines.push(mk_outage_record(
            seq,
            &prev,
            o2_start,
            o2_end,
            AgcomCategory::IrregularService,
            Some(20.0),
        ));
        prev = lines.last().unwrap()["hash"].as_str().unwrap().to_owned();
        seq += 1;

        // Samples day 2
        for h in &[6, 8, 10, 12] {
            lines.push(mk_sample_record(
                seq,
                &prev,
                &format!("2026-07-12T{:02}:00:00Z", h),
                None,
            ));
            prev = lines.last().unwrap()["hash"].as_str().unwrap().to_owned();
            seq += 1;
        }

        // Outage 3: CompleteInterruption, 3 hours (crosses hour band)
        let o3_start = datetime!(2026-07-12 05:00:00 UTC);
        let o3_end = datetime!(2026-07-12 08:00:00 UTC);
        lines.push(mk_outage_record(
            seq,
            &prev,
            o3_start,
            o3_end,
            AgcomCategory::CompleteInterruption,
            None,
        ));

        lines
    }

    // ------------------------------------------------------------------
    // Tests
    // ------------------------------------------------------------------

    #[test]
    fn fixture_chain_intact() {
        let lines = build_fixture();
        let status = verify_chain(&lines);
        assert!(
            status.intact,
            "fixture chain should be intact: {:?}",
            status
        );
    }

    #[test]
    fn fixture_totals() {
        let lines = build_fixture();
        let charter = ServiceCharter {
            repair_window_days: 0,
        };
        let tariff = Tariff {
            complete_per_day: 100.0,
            irregular_per_day: 50.0,
        };
        let cfg = DossierConfig {
            daytime_start: 8,
            daytime_end: 20,
            temp_threshold: 35.0,
        };

        let dossier = project(&lines, &charter, &tariff, &cfg);

        // Chain intact
        assert!(dossier.chain_status.intact);

        // Outage count: 3
        assert_eq!(dossier.outage_count, 3);

        // Total downtime:
        //   O1: 6h = 6*3600 = 21600s
        //   O2: 2h = 7200s
        //   O3: 3h = 10800s
        //   Total: 11h = 39600s
        let expected_total = StdDuration::from_secs(6 * 3600 + 2 * 3600 + 3 * 3600);
        assert_eq!(
            dossier.total_downtime.as_secs(),
            expected_total.as_secs(),
            "total downtime mismatch"
        );

        // Days observed: spans ~2.3 days (July 10 06:00 to July 12 12:00)
        assert!(
            (dossier.days_observed - 2.25).abs() < 0.02,
            "days_observed={}",
            dossier.days_observed
        );

        // Outage 1: 6h CompleteInterruption at 100/day → 0.25 days × 100 = 25.00
        // Outage 2: 2h IrregularService at 50/day → 0.08333 days × 50 ≈ 4.1667
        // Outage 3: 3h CompleteInterruption at 100/day → 0.125 days × 100 = 12.50
        assert_eq!(dossier.per_event_indemnities.len(), 3);
        let ind1 = &dossier.per_event_indemnities[0];
        assert_eq!(ind1.category, "complete_interruption");
        assert!(
            (ind1.indemnity - 25.0).abs() < 0.01,
            "indemnity 1 = {}",
            ind1.indemnity
        );
        println!("Indemnity 1: {} ({})", ind1.indemnity, ind1.formula);
        assert!(ind1.formula.contains("100.00"));

        let ind2 = &dossier.per_event_indemnities[1];
        assert_eq!(ind2.category, "irregular_service");
        assert!(
            (ind2.indemnity - 4.1667).abs() < 0.01,
            "indemnity 2 = {}",
            ind2.indemnity
        );

        let ind3 = &dossier.per_event_indemnities[2];
        assert_eq!(ind3.category, "complete_interruption");
        assert!(
            (ind3.indemnity - 12.50).abs() < 0.01,
            "indemnity 3 = {}",
            ind3.indemnity
        );

        // Temperature correlation:
        //   Daytime window 8-20, threshold 35°C
        //   O1: 13-19, fully in daytime (6h). Samples: 12(36°C), 14(36°C), 16(36°C) >35
        //       Each hour segment uses preceding sample. 13:00→12:00(36°C), 14:00→14:00(36°C),
        //       15:00→14:00(36°C), 16:00→16:00(36°C), 17:00→16:00(36°C), 18:00→16:00(36°C)
        //       -> all 6h above threshold
        //   O2: 8-10, fully in daytime (2h). Samples at 8(20°C) — NOT above threshold
        //       -> 2h in window, 0h above threshold
        //   O3: 5-8, 3h total. Only 8:00 is in daytime window (hour 8).
        //       8:00 is at boundary (included). But O3 has no temp data (temp_c=None).
        //       -> 0h counted as in window (temp_c=None means no temp data for this period)
        //   Total in window: 6h (O1) + 2h (O2) = 8h
        //   Total above threshold in window: 6h (O1) = 6h
        let tc = dossier.temp_correlation.as_ref().unwrap();
        assert_eq!(
            tc.downtime_in_window.as_secs(),
            8 * 3600,
            "downtime_in_window={}",
            tc.downtime_in_window.as_secs()
        );
        assert_eq!(
            tc.downtime_above_threshold.as_secs(),
            6 * 3600,
            "downtime_above_threshold={}",
            tc.downtime_above_threshold.as_secs()
        );
        assert!((tc.share_above_threshold - 0.75).abs() < 0.01);

        // Hour bands:
        //   O1: 13-19 → 6h: hours 13,14,15,16,17 -> band 2 (12-17): 5h;
        //                                      hour 18 -> band 3 (18-24): 1h
        //   O2: 8-10  → 2h: hours 8,9 -> band 1 (6-11): 2h
        //   O3: 5-8   → 3h: hour 5 -> band 0 (0-5): 1h;
        //                     hours 6,7 -> band 1 (6-11): 2h
        //   Total band 0: 1h  |  band 1: 2h+2h = 4h
        //   Total band 2: 5h  |  band 3: 1h
        let bands = &dossier.downtime_by_hour_band;
        assert_eq!(
            bands[0].as_secs(),
            1 * 3600,
            "band 0 = {}",
            bands[0].as_secs()
        );
        assert_eq!(
            bands[1].as_secs(),
            4 * 3600,
            "band 1 = {}",
            bands[1].as_secs()
        );
        assert_eq!(
            bands[2].as_secs(),
            5 * 3600,
            "band 2 = {}",
            bands[2].as_secs()
        );
        assert_eq!(
            bands[3].as_secs(),
            1 * 3600,
            "band 3 = {}",
            bands[3].as_secs()
        );
    }

    #[test]
    fn indemnity_with_repair_window() {
        let lines = build_fixture();
        let charter = ServiceCharter {
            repair_window_days: 1, // 1 day grace period
        };
        let tariff = Tariff {
            complete_per_day: 100.0,
            irregular_per_day: 50.0,
        };
        let cfg = DossierConfig {
            daytime_start: 0,
            daytime_end: 24,
            temp_threshold: 50.0,
        };

        let dossier = project(&lines, &charter, &tariff, &cfg);

        // O1: 6h = 0.25 days. After 1 day repair window → 0 indemnity
        // O2: 2h = 0.0833 days. After 1 day → 0 indemnity
        // O3: 3h = 0.125 days. After 1 day → 0 indemnity
        for ind in &dossier.per_event_indemnities {
            assert!(
                ind.indemnity < 0.001,
                "indemnity should be near zero with 1-day window: {} ({})",
                ind.indemnity,
                ind.formula,
            );
            assert!(ind.formula.contains("max(0"));
        }
    }

    #[test]
    fn broken_chain_reported() {
        let mut lines = build_fixture();
        // Tamper with the first outage record's seq.
        if let Some(Value::Object(m)) = lines.get_mut(12) {
            m.insert("seq".into(), json!(999));
        }
        let charter = ServiceCharter {
            repair_window_days: 0,
        };
        let tariff = Tariff {
            complete_per_day: 100.0,
            irregular_per_day: 50.0,
        };
        let cfg = DossierConfig {
            daytime_start: 0,
            daytime_end: 24,
            temp_threshold: 50.0,
        };
        let dossier = project(&lines, &charter, &tariff, &cfg);
        assert!(!dossier.chain_status.intact);
        assert!(dossier.chain_status.break_at.is_some());
    }
}
