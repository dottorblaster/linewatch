//! Optional OpenTelemetry OTLP metrics exporter.
//!
//! When configured with an endpoint, this module creates a
//! [`MeterProvider`] with a [`PeriodicReader`] that exports gauge,
//! counter, and histogram instruments via HTTP/protobuf to an OTLP
//! collector.  When the endpoint is `None` no instrumentation is created
//! and no OTLP code runs.

use std::time::Duration;

use opentelemetry::{KeyValue, metrics::MeterProvider};
use opentelemetry_otlp::MetricExporterBuilder;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

use crate::core::types::{ProbeOutcome, Status};

// ---------------------------------------------------------------------------
// OtlpExporter
// ---------------------------------------------------------------------------

/// Wraps OTel instruments and the meter provider.
///
/// Constructed only when the config provides an `otlp_endpoint`.
pub struct OtlpExporter {
    _provider: SdkMeterProvider, // kept alive for the export interval
    up: opentelemetry::metrics::Gauge<f64>,
    loss: opentelemetry::metrics::Gauge<f64>,
    rtt: opentelemetry::metrics::Histogram<f64>,
    temp: opentelemetry::metrics::Gauge<f64>,
    outages: opentelemetry::metrics::Counter<u64>,
    status_ok: opentelemetry::metrics::Gauge<f64>,
    status_degraded: opentelemetry::metrics::Gauge<f64>,
    status_down: opentelemetry::metrics::Gauge<f64>,
    status_local_or_power: opentelemetry::metrics::Gauge<f64>,
    status_dns_fail: opentelemetry::metrics::Gauge<f64>,
    status_http_fail: opentelemetry::metrics::Gauge<f64>,
}

impl OtlpExporter {
    /// Create the OTLP exporter and start the background export loop.
    ///
    /// Returns `None` when `endpoint` is empty — this allows the caller to
    /// pass an `Option<String>` directly.
    pub fn new(endpoint: Option<&str>) -> Option<Self> {
        let endpoint = endpoint?;
        if endpoint.is_empty() {
            return None;
        }

        let exporter = MetricExporterBuilder::new()
            .with_http()
            .with_endpoint(endpoint.to_string())
            .build()
            .ok()?;

        let reader = PeriodicReader::builder(exporter)
            .with_interval(Duration::from_secs(10))
            .build();

        let provider: SdkMeterProvider = SdkMeterProvider::builder().with_reader(reader).build();

        let meter = provider.meter("linewatch");

        let up = meter
            .f64_gauge("linewatch.target.up")
            .with_description("Target reachable (1 = up, 0 = down)")
            .build();

        let loss = meter
            .f64_gauge("linewatch.target.loss_pct")
            .with_description("Packet loss percentage toward this target")
            .build();

        let rtt = meter
            .f64_histogram("linewatch.target.rtt_ms")
            .with_description("Round-trip time in milliseconds toward this target")
            .build();

        let temp = meter
            .f64_gauge("linewatch.temperature_c")
            .with_description("Current ambient temperature in °C")
            .build();

        let outages = meter
            .u64_counter("linewatch.outages.total")
            .with_description("Outage events by category")
            .build();

        let status_ok = meter
            .f64_gauge("linewatch.status")
            .with_description(
                "Current status: 0=ok 1=degraded 2=down 3=local_or_power 4=dns_fail 5=http_fail",
            )
            .build();
        let status_degraded = status_ok.clone();
        let status_down = status_ok.clone();
        let status_local_or_power = status_ok.clone();
        let status_dns_fail = status_ok.clone();
        let status_http_fail = status_ok.clone();

        Some(Self {
            _provider: provider,
            up,
            loss,
            rtt,
            temp,
            outages,
            status_ok,
            status_degraded,
            status_down,
            status_local_or_power,
            status_dns_fail,
            status_http_fail,
        })
    }

    // -- Per-cycle updates --------------------------------------------------

    /// Record a per-target probe outcome.
    pub fn record_target(&self, target: &str, outcome: &ProbeOutcome) {
        let attrs = [KeyValue::new("target", target.to_owned())];

        self.up
            .record(if outcome.reachable { 1.0 } else { 0.0 }, &attrs);
        self.loss.record(f64::from(outcome.loss_pct), &attrs);
        if let Some(rtt) = outcome.rtt {
            self.rtt.record(rtt.as_secs_f64() * 1000.0, &attrs);
        }
    }

    /// Record the current temperature.
    pub fn record_temp(&self, temp_c: Option<f64>) {
        if let Some(t) = temp_c {
            self.temp.record(t, &[]);
        }
    }

    /// Increment the outage counter.
    pub fn record_outage(&self, category: &str) {
        self.outages
            .add(1, &[KeyValue::new("category", category.to_owned())]);
    }

    /// Record the current status — sets one gauge to its ordinal value
    /// and the others to 0.
    pub fn record_status(&self, status: &Status) {
        let (ord, gauge) = match status {
            Status::Ok => (0.0, &self.status_ok),
            Status::Degraded => (1.0, &self.status_degraded),
            Status::Down => (2.0, &self.status_down),
            Status::LocalOrPower => (3.0, &self.status_local_or_power),
            Status::DnsFail => (4.0, &self.status_dns_fail),
            Status::HttpFail => (5.0, &self.status_http_fail),
        };
        // Reset all to 0, then set the active one.
        for g in &[
            &self.status_ok,
            &self.status_degraded,
            &self.status_down,
            &self.status_local_or_power,
            &self.status_dns_fail,
            &self.status_http_fail,
        ] {
            g.record(0.0, &[]);
        }
        gauge.record(ord, &[]);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::TargetKind;
    use std::time::Duration;

    #[test]
    fn new_with_none_returns_none() {
        assert!(OtlpExporter::new(None).is_none());
        assert!(OtlpExporter::new(Some("")).is_none());
    }

    #[test]
    fn new_with_dummy_endpoint_does_not_panic() {
        // Set a high-enough port so the export attempt fails silently.
        let o = OtlpExporter::new(Some("http://127.0.0.1:1/v1/metrics"));
        // The exporter may or may not build depending on connectivity, but
        // it should never panic.
        if let Some(exporter) = o {
            let outcome = ProbeOutcome {
                kind: TargetKind::Http,
                reachable: true,
                rtt: Some(Duration::from_millis(10)),
                loss_pct: 0,
            };
            exporter.record_target("test", &outcome);
            exporter.record_temp(Some(22.0));
            exporter.record_outage("irregular_service");
            exporter.record_status(&Status::Ok);
            // No assertion — just verifying no crash.
        }
    }
}
