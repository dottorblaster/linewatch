//! PDF dossier renderer.
//!
//! Uses [`genpdf`] to produce a paginated PDF from a [`Dossier`] and the
//! parsed records.  Font: bundled `assets/DejaVuSans.ttf`.

use std::path::Path;

use genpdf::elements::{
    Break, FrameCellDecorator, Image, LinearLayout, Paragraph, StyledElement, TableLayout,
};
use genpdf::{Document, Element};
use sha2::{Digest, Sha256};

use crate::core::chain::Record;
use crate::core::dossier::Dossier;
use crate::core::events::AgcomCategory;

/// Render the dossier to a PDF file.
pub fn render_pdf(
    dossier: &Dossier,
    records: &[Record],
    chart_path: Option<&Path>,
    output: &Path,
) -> Result<(), genpdf::error::Error> {
    // --- Load font ---
    let font_data = include_bytes!("../../assets/DejaVuSans.ttf").to_vec();
    let regular = genpdf::fonts::FontData::new(font_data, None)?;
    let family = genpdf::fonts::FontFamily {
        regular: regular.clone(),
        bold: regular.clone(),
        italic: regular.clone(),
        bold_italic: regular,
    };

    let mut doc = Document::new(family);
    doc.set_title("Linewatch Dossier");
    doc.set_paper_size(genpdf::PaperSize::A4);

    // --- Title ---
    doc.push(StyledElement::new(
        Paragraph::new("Linewatch Dossier"),
        genpdf::style::Style::new().bold().with_font_size(22),
    ));
    doc.push(Break::new(1));

    // --- Executive summary ---
    doc.push(StyledElement::new(
        Paragraph::new("Executive Summary"),
        genpdf::style::Style::new().bold().with_font_size(16),
    ));
    doc.push(Break::new(0.5));

    let total_secs = dossier.total_downtime.as_secs_f64();
    let mut summary = format!(
        "Days observed: {:.2}\nOutage count: {}\nTotal downtime: {:.1} hours ({:.2} days)\n\n",
        dossier.days_observed,
        dossier.outage_count,
        total_secs / 3600.0,
        total_secs / 86400.0,
    );
    let band_labels = ["00:00-06:00", "06:00-12:00", "12:00-18:00", "18:00-24:00"];
    for (i, label) in band_labels.iter().enumerate() {
        let s = dossier.downtime_by_hour_band[i].as_secs_f64() / 3600.0;
        summary.push_str(&format!("  - {}: {:.1} h\n", label, s));
    }
    if let Some(ref tc) = dossier.temp_correlation {
        let win_hrs = tc.downtime_in_window.as_secs_f64() / 3600.0;
        let hot_hrs = tc.downtime_above_threshold.as_secs_f64() / 3600.0;
        let pct = tc.share_above_threshold * 100.0;
        summary.push_str(&format!(
            "\nTemperature correlation: {:.1} of {:.1} daytime downtime hours ({:.0}%) above threshold.\n",
            hot_hrs, win_hrs, pct,
        ));
    }
    doc.push(Paragraph::new(summary));
    doc.push(Break::new(0.5));

    // --- Chart image ---
    if let Some(path) = chart_path {
        let img = Image::from_path(path)?;
        let mut layout = LinearLayout::vertical();
        layout.push(img);
        doc.push(layout);
        doc.push(Break::new(0.5));
    }

    // --- Chain integrity ---
    doc.push(StyledElement::new(
        Paragraph::new("Chain Integrity"),
        genpdf::style::Style::new().bold().with_font_size(16),
    ));
    doc.push(Break::new(0.5));

    let cs = &dossier.chain_status;
    if cs.intact {
        doc.push(Paragraph::new("Status: OK - Intact"));
    } else {
        doc.push(Paragraph::new(format!(
            "Status: BROKEN at seq {}",
            cs.break_at.unwrap_or(0)
        )));
    }
    if let (Some(first), Some(last)) = (records.first(), records.last()) {
        doc.push(Paragraph::new(format!(
            "Record range: seq {} - {}",
            get_seq(first),
            get_seq(last)
        )));
        doc.push(Paragraph::new(format!("Head hash: {}", get_hash(last))));
    }
    doc.push(Paragraph::new("Re-verify: cargo run -- report"));
    doc.push(Break::new(1));

    // --- Event table ---
    doc.push(StyledElement::new(
        Paragraph::new("Chronological Events"),
        genpdf::style::Style::new().bold().with_font_size(16),
    ));
    doc.push(Break::new(0.5));

    let mut table = TableLayout::new(vec![1, 3, 3, 2, 3, 2]);
    table.set_cell_decorator(FrameCellDecorator::new(true, true, false));
    let _ = table.push_row(vec![
        Box::new(Paragraph::new("#")) as Box<dyn Element>,
        Box::new(Paragraph::new("Started")) as Box<dyn Element>,
        Box::new(Paragraph::new("Ended")) as Box<dyn Element>,
        Box::new(Paragraph::new("Dur. (h)")) as Box<dyn Element>,
        Box::new(Paragraph::new("Category")) as Box<dyn Element>,
        Box::new(Paragraph::new("Indemnity")) as Box<dyn Element>,
    ]);
    for (i, ind) in dossier.per_event_indemnities.iter().enumerate() {
        let dur_hrs = ind.duration_days * 24.0;
        let _ = table.push_row(vec![
            Box::new(Paragraph::new(format!("{}", i + 1))) as Box<dyn Element>,
            Box::new(Paragraph::new(&ind.event_start)) as Box<dyn Element>,
            Box::new(Paragraph::new(&ind.event_end)) as Box<dyn Element>,
            Box::new(Paragraph::new(format!("{:.2}", dur_hrs))) as Box<dyn Element>,
            Box::new(Paragraph::new(&ind.category)) as Box<dyn Element>,
            Box::new(Paragraph::new(format!("{:.2}", ind.indemnity))) as Box<dyn Element>,
        ]);
    }
    doc.push(table);
    doc.push(Break::new(1));

    // --- Indemnity summary ---
    doc.push(StyledElement::new(
        Paragraph::new("Indemnity Summary"),
        genpdf::style::Style::new().bold().with_font_size(16),
    ));
    doc.push(Break::new(0.5));

    let total_indemnity: f64 = dossier
        .per_event_indemnities
        .iter()
        .map(|l| l.indemnity)
        .sum();
    doc.push(Paragraph::new(format!(
        "Total estimated indemnity: {:.2}",
        total_indemnity
    )));
    if let Some(ind) = dossier.per_event_indemnities.first() {
        doc.push(Paragraph::new(format!(
            "Repair window: {} day(s)",
            ind.repair_window_days
        )));
        doc.push(Paragraph::new(format!(
            "Tariff (complete / irregular): {:.2} / {:.2} per day",
            ind.tariff_daily, ind.tariff_daily
        )));
    }
    doc.push(Break::new(1));

    // --- Appendix: traceroute hops ---
    doc.push(StyledElement::new(
        Paragraph::new("Appendix A - Traceroute Hops"),
        genpdf::style::Style::new().bold().with_font_size(16),
    ));
    doc.push(Break::new(0.5));

    for record in records {
        if let Record::Outage { event, hops, .. } = record {
            let cat = match event.category {
                AgcomCategory::CompleteInterruption => "CompleteInterruption",
                AgcomCategory::IrregularService => "IrregularService",
            };
            doc.push(Paragraph::new(format!(
                "Event: {} - {}",
                event.started.date(),
                cat
            )));
            doc.push(Break::new(0.3));

            if hops.is_empty() {
                doc.push(Paragraph::new("(no hop data)"));
            } else {
                let mut hop_table = TableLayout::new(vec![1, 3, 2]);
                hop_table.set_cell_decorator(FrameCellDecorator::new(true, true, false));
                let _ = hop_table.push_row(vec![
                    Box::new(Paragraph::new("TTL")) as Box<dyn Element>,
                    Box::new(Paragraph::new("Address")) as Box<dyn Element>,
                    Box::new(Paragraph::new("RTT")) as Box<dyn Element>,
                ]);
                for hop in hops {
                    let addr = hop
                        .addr
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| "*".to_string());
                    let rtt = hop
                        .rtt
                        .map(|d| format!("{:.1} ms", d.as_secs_f64() * 1000.0))
                        .unwrap_or_else(|| "-".to_string());
                    let _ = hop_table.push_row(vec![
                        Box::new(Paragraph::new(format!("{}", hop.ttl))) as Box<dyn Element>,
                        Box::new(Paragraph::new(addr)) as Box<dyn Element>,
                        Box::new(Paragraph::new(rtt)) as Box<dyn Element>,
                    ]);
                }
                doc.push(hop_table);
                doc.push(Break::new(0.3));
            }
        }
    }
    doc.push(Break::new(1));

    // --- Footer with chain head hash ---
    let head_hash = records.last().map(get_hash).unwrap_or("");
    doc.push(Paragraph::new(format!("Chain head hash: {}", head_hash)));

    // Render the document.
    doc.render_to_file(output)?;

    // Compute and print the PDF fingerprint.
    let pdf_bytes = std::fs::read(output).map_err(|e| {
        genpdf::error::Error::new(
            "cannot read output for hashing",
            genpdf::error::ErrorKind::IoError(e.into()),
        )
    })?;
    let fingerprint = hex::encode(Sha256::digest(&pdf_bytes));
    eprintln!("PDF fingerprint (SHA-256): {}", fingerprint);

    Ok(())
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
    fn render_fixture_pdf() {
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
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        let output = std::env::temp_dir().join("linewatch_fixture.pdf");
        render_pdf(&dossier, &records, None, &output).expect("PDF rendering should succeed");
        println!("PDF written to {:?}", output);
        assert!(output.exists(), "PDF file should exist");
        let meta = std::fs::metadata(&output).unwrap();
        assert!(meta.len() > 1000, "PDF should be larger than 1 KB");
    }

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

        lines
    }
}
