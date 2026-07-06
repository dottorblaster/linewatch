//! PNG timeline chart — temperature against outage spans.
//!
//! Uses plotters with a bundled DejaVuSans TTF font to avoid musl fontconfig
//! issues.  The output is a PNG written to the specified path.

use std::path::Path;

use plotters::prelude::*;

use serde_json::Value;
use time::OffsetDateTime;

use crate::core::chain::Record;
use crate::core::dossier::Dossier;
use crate::core::events::AgcomCategory;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Render a temperature-vs-outage timeline PNG from raw log lines and the
/// computed [`Dossier`].
///
/// `lines` — the deserialized JSONL values (used to extract the raw
/// temperature time series).
/// `dossier` — pre-computed summary (used for outage-event metadata).
/// `output` — filesystem path for the generated PNG.
#[allow(dead_code)]
pub fn render_chart(
    lines: &[Value],
    _dossier: &Dossier,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // Register the bundled font.
    // Try to register the bundled font; if it fails (e.g. ab_glyph feature
    // unavailable), we proceed without it and let plotters use its default.
    let _ = plotters::style::register_font(
        "DejaVu Sans",
        FontStyle::Normal,
        include_bytes!("../../assets/DejaVuSans.ttf"),
    );

    // Parse all records.
    let records: Vec<Record> = lines
        .iter()
        .filter_map(|v| serde_json::from_value::<Record>(v.clone()).ok())
        .collect();

    // Collect temperature samples: (unix_seconds, temp_c).
    let temp_data: Vec<(f64, f64)> = records
        .iter()
        .filter_map(|r| match r {
            Record::Sample { ts, temp_c, .. } => {
                let t = OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339)
                    .ok()?;
                Some((t.unix_timestamp() as f64, (*temp_c)?))
            }
            _ => None,
        })
        .collect();

    if temp_data.is_empty() {
        return Err("no temperature data to chart".into());
    }

    // Collect outage spans: (start_unix, end_unix, label).
    let outages: Vec<(f64, f64, &str)> = records
        .iter()
        .filter_map(|r| match r {
            Record::Outage { event, .. } => {
                let end = event.ended.as_ref()?;
                let label = match event.category {
                    AgcomCategory::CompleteInterruption => "CompleteInterruption",
                    AgcomCategory::IrregularService => "IrregularService",
                };
                Some((
                    event.started.unix_timestamp() as f64,
                    end.unix_timestamp() as f64,
                    label,
                ))
            }
            _ => None,
        })
        .collect();

    // Determine X range (with padding).
    let min_x = temp_data.first().unwrap().0;
    let max_x = temp_data.last().unwrap().0;
    let span = (max_x - min_x).max(3600.0);
    let pad = span * 0.05;
    let x_min = min_x - pad;
    let x_max = max_x + pad;

    // Temperature range (with padding).
    let temps: Vec<f64> = temp_data.iter().map(|(_, t)| *t).collect();
    let t_low = temps.iter().cloned().fold(f64::INFINITY, f64::min);
    let t_high = temps.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let t_range = (t_high - t_low).max(5.0);
    let y_min = (t_low - t_range * 0.1).floor();
    let y_max = (t_high + t_range * 0.1).ceil();

    // --- Build the chart ---
    let root = BitMapBackend::new(output.to_str().unwrap(), (1000, 500)).into_drawing_area();
    root.fill(&WHITE)?;

    let mut chart = ChartBuilder::on(&root)
        .caption(
            "Temperature & Outage Timeline",
            ("DejaVu Sans", 24).into_font(),
        )
        .margin(15)
        .x_label_area_size(35)
        .y_label_area_size(50)
        .build_cartesian_2d(x_min..x_max, y_min..y_max)?;

    // Custom label formatter for Unix timestamps → readable date.
    let x_fmt = |&x: &f64| -> String {
        let odt = OffsetDateTime::from_unix_timestamp(x as i64).ok();
        match odt {
            Some(t) => format!("{:02}/{:02} {:02}:00", t.month() as u8, t.day(), t.hour()),
            None => format!("{:.0}", x),
        }
    };

    chart
        .configure_mesh()
        .x_desc("Time")
        .y_desc("Temperature (°C)")
        .x_label_formatter(&x_fmt)
        .axis_desc_style(("DejaVu Sans", 14).into_font())
        .label_style(("DejaVu Sans", 12).into_font())
        .draw()?;

    // --- Draw outage spans as shaded rectangles ---
    for (x1, x2, label) in &outages {
        let color: RGBColor = match *label {
            "CompleteInterruption" => RGBColor(220, 50, 50),
            "IrregularService" => RGBColor(255, 165, 0),
            _ => RGBColor(200, 0, 200),
        };
        chart.draw_series(std::iter::once(Rectangle::new(
            [(*x1, y_min), (*x2, y_max)],
            color.mix(0.2).filled(),
        )))?;

        let mid = (x1 + x2) / 2.0;
        let label_y = y_max - (y_max - y_min) * 0.05;
        chart.draw_series(std::iter::once(Text::new(
            *label,
            (mid, label_y),
            ("DejaVu Sans", 10).into_font().color(&BLACK),
        )))?;
    }

    // --- Draw temperature line ---
    chart
        .draw_series(LineSeries::new(
            temp_data.iter().map(|(x, y)| (*x, *y)),
            BLUE,
        ))?
        .label("Temperature")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], BLUE));

    // --- Temperature threshold line ---
    let threshold = 35.0;
    if threshold >= y_min && threshold <= y_max {
        chart.draw_series(std::iter::once(PathElement::new(
            vec![(x_min, threshold), (x_max, threshold)],
            RED.mix(0.5).stroke_width(1),
        )))?;
        let label_y = threshold - (y_max - y_min) * 0.03;
        chart.draw_series(std::iter::once(Text::new(
            format!("{threshold}°C"),
            (x_min, label_y),
            ("DejaVu Sans", 11).into_font().color(&RED),
        )))?;
    }

    // --- Daytime window indicator (green bars at bottom) ---
    let day_start_hour = 8.0;
    let day_end_hour = 20.0;
    let bar_y = y_min + (y_max - y_min) * 0.01;
    let bar_h = (y_max - y_min) * 0.025;
    let one_hour_secs: f64 = 3600.0;

    let mut cursor = x_min;
    while cursor < x_max {
        // Convert to hour of day (UTC).
        let ts = match OffsetDateTime::from_unix_timestamp(cursor as i64) {
            Ok(t) => t,
            Err(_) => break,
        };
        let hour = ts.hour() as f64 + ts.minute() as f64 / 60.0;
        let band_end = cursor + one_hour_secs;
        if hour >= day_start_hour && hour < day_end_hour {
            chart.draw_series(std::iter::once(Rectangle::new(
                [(cursor, bar_y), (band_end, bar_y + bar_h)],
                GREEN.mix(0.4).filled(),
            )))?;
        }
        cursor = band_end;
    }

    root.present()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::chain::{RecordChain, compute_hash};
    use crate::core::dossier::{DossierConfig, ServiceCharter, Tariff, project};
    use crate::core::events::OutageEvent;
    use crate::core::types::Status;
    use time::macros::datetime;

    #[test]
    fn render_fixture_chart() {
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

        let output = std::env::temp_dir().join("linewatch_fixture_chart.png");
        render_chart(&lines, &dossier, &output).expect("chart rendering should succeed");

        println!("Chart written to {:?}", output);
        assert!(output.exists(), "PNG file should exist");

        let meta = std::fs::metadata(&output).unwrap();
        assert!(meta.len() > 1000, "PNG should be larger than 1 KB");
    }

    // ------------------------------------------------------------------
    fn push_record(
        lines: &mut Vec<Value>,
        seq: &mut u64,
        prev: &mut String,
        body: &impl serde::Serialize,
    ) {
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

    fn build_fixture_lines() -> Vec<Value> {
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

        // Outage 1: CompleteInterruption 13–19
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
                    hops: vec![],
                },
            );
        }

        // Samples day 2
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
                    ts: format!("2026-07-11T{:02}:00:00Z", h),
                    temp_c: Some(if *h >= 12 { 38.0 } else { 20.0 }),
                    outcomes: vec![],
                },
            );
        }

        // Outage 2: IrregularService 8–10
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
                        started: datetime!(2026-07-11 08:00:00 UTC),
                        ended: Some(datetime!(2026-07-11 10:00:00 UTC)),
                        worst_status: Status::Degraded,
                        min_temp_c: Some(20.0),
                        samples_count: 0,
                        category: AgcomCategory::IrregularService,
                    },
                    hops: vec![],
                },
            );
        }

        // Samples day 3
        for h in &[6, 8, 10, 12] {
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
                    ts: format!("2026-07-12T{:02}:00:00Z", h),
                    temp_c: None,
                    outcomes: vec![],
                },
            );
        }

        // Outage 3: CompleteInterruption 5–8
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
                        started: datetime!(2026-07-12 05:00:00 UTC),
                        ended: Some(datetime!(2026-07-12 08:00:00 UTC)),
                        worst_status: Status::Down,
                        min_temp_c: None,
                        samples_count: 0,
                        category: AgcomCategory::CompleteInterruption,
                    },
                    hops: vec![],
                },
            );
        }

        lines
    }
}
