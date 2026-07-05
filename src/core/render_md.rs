//! Markdown dossier renderer.
//!
//! [`render_markdown`] takes a [`Dossier`] and the parsed records, and
//! returns a complete Markdown document with executive summary, outage
//! table, indemnity breakdown, appendices, and a self-hash footer.

use std::fmt::Write;

use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::core::chain::Record;
use crate::core::dossier::{Dossier, IndemnityLine};
use crate::core::events::AgcomCategory;

/// Configuration for the rendered report.
pub struct ReportConfig {
    /// Relative path to the optional chart image (e.g. `"chart.png"`).
    pub chart_path: Option<String>,
}

/// Render a complete Markdown dossier.
pub fn render_markdown(dossier: &Dossier, records: &[Record], cfg: &ReportConfig) -> String {
    let mut md = String::new();

    // ------------------------------------------------------------------
    // Header
    // ------------------------------------------------------------------
    let _ = writeln!(md, "# Linewatch Dossier\n");

    // ------------------------------------------------------------------
    // Executive summary
    // ------------------------------------------------------------------
    let _ = writeln!(md, "## Executive Summary\n");

    let _ = writeln!(md, "- **Days observed:** {:.2}", dossier.days_observed);
    let _ = writeln!(md, "- **Outage count:** {}", dossier.outage_count);
    let total_secs = dossier.total_downtime.as_secs_f64();
    let _ = writeln!(
        md,
        "- **Total downtime:** {:.1} hours ({:.2} days)",
        total_secs / 3600.0,
        total_secs / 86400.0,
    );

    // Per-hour-band breakdown.
    let _ = writeln!(md, "- **Downtime by time-of-day band:**");
    let band_labels = ["00:00-06:00", "06:00-12:00", "12:00-18:00", "18:00-24:00"];
    for (i, label) in band_labels.iter().enumerate() {
        let s = dossier.downtime_by_hour_band[i].as_secs_f64() / 3600.0;
        let _ = writeln!(md, "  - *{}:* {:.1} h", label, s);
    }

    // Temperature correlation sentence.
    if let Some(ref tc) = dossier.temp_correlation {
        let win_hrs = tc.downtime_in_window.as_secs_f64() / 3600.0;
        let hot_hrs = tc.downtime_above_threshold.as_secs_f64() / 3600.0;
        let pct = tc.share_above_threshold * 100.0;
        let _ = writeln!(
            md,
            "- **Temperature correlation:** {:.1} of {:.1} downtime hours \
             occurred within the daytime window; **{:.0}%** of that \
             window downtime was above the temperature threshold.",
            hot_hrs, win_hrs, pct,
        );
    }

    let _ = writeln!(md);

    // ------------------------------------------------------------------
    // Chart reference
    // ------------------------------------------------------------------
    if let Some(ref path) = cfg.chart_path {
        let _ = writeln!(md, "![Temperature & Outage Timeline]({})\n", path);
    }

    // ------------------------------------------------------------------
    // Chain integrity
    // ------------------------------------------------------------------
    let _ = writeln!(md, "## Chain Integrity\n");
    let cs = &dossier.chain_status;
    if cs.intact {
        let _ = writeln!(md, "- **Status:** OK - Intact");
    } else {
        let _ = writeln!(
            md,
            "- **Status:** BROKEN at seq {}",
            cs.break_at.unwrap_or(0),
        );
    }

    if let (Some(first), Some(last)) = (records.first(), records.last()) {
        let first_seq = get_seq(first);
        let last_seq = get_seq(last);
        let _ = writeln!(md, "- **Record range:** seq {} - {}", first_seq, last_seq);
        let head_hash = get_hash(last);
        let _ = writeln!(md, "- **Head hash:** `{}`", head_hash);
    }

    let _ = writeln!(
        md,
        "- **Re-verify command:** `cargo run -- report` \
         (or independently validate the hash chain)"
    );
    let _ = writeln!(md);

    // ------------------------------------------------------------------
    // Event table
    // ------------------------------------------------------------------
    let _ = writeln!(md, "## Chronological Events\n");
    let _ = writeln!(
        md,
        concat!(
            "| # | Start | End | Duration (h) | Category | ",
            "Worst Status | Indemnity | Formula |",
        )
    );
    let _ = writeln!(
        md,
        concat!(
            "|---|-------|-----|-------------|----------|",
            "-------------|----------|---------|",
        )
    );

    for (i, ind) in dossier.per_event_indemnities.iter().enumerate() {
        let dur_hrs = ind.duration_days * 24.0;
        let _ = writeln!(
            md,
            concat!("| {} | {} | {} | {:.2} | {} | {} | {:.2} | `{}` |",),
            i + 1,
            ind.event_start,
            ind.event_end,
            dur_hrs,
            ind.category,
            "",
            ind.indemnity,
            ind.formula,
        );
    }

    if dossier.per_event_indemnities.is_empty() {
        let _ = writeln!(md, concat!("| - | - | - | - | - | - | 0.00 | - |"));
    }
    let _ = writeln!(md);

    // ------------------------------------------------------------------
    // Indemnity summary
    // ------------------------------------------------------------------
    let _ = writeln!(md, "## Indemnity Summary\n");
    let total_indemnity: f64 = dossier
        .per_event_indemnities
        .iter()
        .map(|l| l.indemnity)
        .sum();
    let _ = writeln!(md, concat!("| Metric | Value |"));
    let _ = writeln!(md, concat!("|--------|-------|"));
    let _ = writeln!(
        md,
        "| Total estimated indemnity | **{:.2}** |",
        total_indemnity
    );
    if let Some(ind) = dossier.per_event_indemnities.first() {
        let rw = ind.repair_window_days;
        let td = ind.tariff_daily;
        let _ = writeln!(md, "| Repair window | {} day(s) |", rw);
        let _ = writeln!(
            md,
            "| Tariff (complete / irregular) | {:.2} / {:.2} per day |",
            td, td
        );
    }
    let _ = writeln!(md);

    // ------------------------------------------------------------------
    // Appendix: per-event traceroute hops
    // ------------------------------------------------------------------
    let _ = writeln!(md, "## Appendix A - Traceroute Hops\n");
    let mut had_hops = false;
    for record in records {
        if let Record::Outage { event, hops, .. } = record {
            let cat = match event.category {
                AgcomCategory::CompleteInterruption => "CompleteInterruption",
                AgcomCategory::IrregularService => "IrregularService",
            };
            let _ = writeln!(md, "### Event: {} - {}\n", event.started.date(), cat,);
            let _ = writeln!(md, concat!("| TTL | Address | RTT |"));
            let _ = writeln!(md, concat!("|-----|---------|-----|"));

            if hops.is_empty() {
                let _ = writeln!(md, concat!("| - | (no hop data) | - |"));
            } else {
                had_hops = true;
                for hop in hops {
                    let addr = hop
                        .addr
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| "*".to_string());
                    let rtt = hop
                        .rtt
                        .map(|d| format!("{:.1} ms", d.as_secs_f64() * 1000.0))
                        .unwrap_or_else(|| "-".to_string());
                    let _ = writeln!(md, concat!("| {} | {} | {} |"), hop.ttl, addr, rtt,);
                }
            }
            let _ = writeln!(md);
        }
    }
    if !had_hops {
        let _ = writeln!(md, "*No traceroute data recorded.*\n");
    }

    // ------------------------------------------------------------------
    // Self-hash
    // ------------------------------------------------------------------
    let hash = hex::encode(Sha256::digest(md.as_bytes()));
    let _ = writeln!(md, "---\n");
    let _ = writeln!(md, "**Document fingerprint:** `{}`", hash);

    md
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn get_seq(r: &Record) -> u64 {
    match r {
        Record::Sample { chain, .. }
        | Record::Outage { chain, .. }
        | Record::MonitorRestart { chain } => chain.seq,
    }
}

fn get_hash(r: &Record) -> &str {
    match r {
        Record::Sample { chain, .. }
        | Record::Outage { chain, .. }
        | Record::MonitorRestart { chain } => &chain.hash,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::dossier::{DossierConfig, ServiceCharter, Tariff, project};

    #[test]
    fn render_fixture_markdown() {
        let lines = build_fixture_lines();

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

        let records: Vec<Record> = lines
            .iter()
            .filter_map(|v| serde_json::from_value::<Record>(v.clone()).ok())
            .collect();

        let report_cfg = ReportConfig {
            chart_path: Some("chart.png".into()),
        };

        let md = render_markdown(&dossier, &records, &report_cfg);
        println!("{}", md);

        assert!(md.contains("Linewatch Dossier"));
        assert!(md.contains("Executive Summary"));
        assert!(md.contains("Chain Integrity"));
        assert!(md.contains("Chronological Events"));
        assert!(md.contains("Indemnity Summary"));
        assert!(md.contains("Appendix A"));
        assert!(md.contains("Document fingerprint:"));
        assert!(md.contains("chart.png"));
        assert!(md.contains("Intact"));
        assert!(md.contains("OK"));
    }

    // ------------------------------------------------------------------
    fn push_record(
        lines: &mut Vec<serde_json::Value>,
        seq: &mut u64,
        prev: &mut String,
        body: &impl serde::Serialize,
    ) {
        use crate::core::chain::compute_hash;
        use serde_json::json;
        let prev_str = prev.clone();
        let hash = compute_hash(body, &prev_str);
        let mut v = serde_json::to_value(body).unwrap();
        if let Some(m) = v.as_object_mut() {
            m.insert("hash".into(), json!(hash));
        }
        lines.push(v);
        *prev = hash;
        *seq += 1;
    }

    fn build_fixture_lines() -> Vec<serde_json::Value> {
        use crate::core::chain::RecordChain;
        use crate::core::events::OutageEvent;
        use crate::core::types::Status;
        use crate::shell::trace::Hop;
        use std::net::{IpAddr, Ipv4Addr};
        use std::time::Duration;
        use time::macros::datetime;

        let mut lines = Vec::new();
        let mut prev = String::new();
        let mut seq = 0u64;

        // MonitorRestart
        {
            let s = seq;
            let p = prev.clone();
            push_record(
                &mut lines,
                &mut seq,
                &mut prev,
                &Record::MonitorRestart {
                    chain: RecordChain {
                        seq: s,
                        prev_hash: p,
                        hash: String::new(),
                    },
                },
            );
        }

        // Samples day 1
        for h in &[6, 8, 10, 12, 14, 16] {
            let s = seq;
            let p = prev.clone();
            push_record(
                &mut lines,
                &mut seq,
                &mut prev,
                &Record::Sample {
                    chain: RecordChain {
                        seq: s,
                        prev_hash: p,
                        hash: String::new(),
                    },
                    ts: format!("2026-07-10T{:02}:00:00Z", h),
                    temp_c: Some(if *h >= 12 { 36.0 } else { 22.0 }),
                    outcomes: vec![],
                },
            );
        }

        // Outage 1 with traceroute hops
        {
            let s = seq;
            let p = prev.clone();
            push_record(
                &mut lines,
                &mut seq,
                &mut prev,
                &Record::Outage {
                    chain: RecordChain {
                        seq: s,
                        prev_hash: p,
                        hash: String::new(),
                    },
                    event: OutageEvent {
                        started: datetime!(2026-07-10 13:00:00 UTC),
                        ended: Some(datetime!(2026-07-10 19:00:00 UTC)),
                        worst_status: Status::Down,
                        min_temp_c: Some(36.0),
                        samples_count: 0,
                        category: AgcomCategory::CompleteInterruption,
                    },
                    hops: vec![
                        Hop {
                            ttl: 1,
                            addr: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))),
                            rtt: Some(Duration::from_millis(2)),
                        },
                        Hop {
                            ttl: 2,
                            addr: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
                            rtt: Some(Duration::from_millis(5)),
                        },
                        Hop {
                            ttl: 3,
                            addr: Some(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))),
                            rtt: Some(Duration::from_millis(17)),
                        },
                    ],
                },
            );
        }

        // Close with MonitorRestart
        {
            let s = seq;
            let p = prev.clone();
            push_record(
                &mut lines,
                &mut seq,
                &mut prev,
                &Record::MonitorRestart {
                    chain: RecordChain {
                        seq: s,
                        prev_hash: p,
                        hash: String::new(),
                    },
                },
            );
        }

        lines
    }
}
