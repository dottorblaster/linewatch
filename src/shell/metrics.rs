//! Prometheus metrics and HTTP endpoints.
//!
//! Provides an axum server with routes `/metrics` (Prometheus text format),
//! `/healthz` (always 200), and `/readyz` (200 after validation).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::routing::get;
use prometheus::{
    CounterVec, Gauge, GaugeVec, HistogramVec, Registry, TextEncoder,
    register_counter_vec_with_registry, register_gauge_vec_with_registry,
    register_histogram_vec_with_registry,
};

use crate::core::types::{ProbeOutcome, Status};

// ---------------------------------------------------------------------------
// Metrics state
// ---------------------------------------------------------------------------

/// Wraps all Prometheus metrics and the HTTP server readiness flags.
pub struct Metrics {
    registry: Registry,

    // Per-target metrics (label: "target").
    up: GaugeVec,
    loss: GaugeVec,
    rtt: HistogramVec,

    // Global metrics.
    temp: Gauge,
    outages: CounterVec, // label: "category"
    status: GaugeVec,    // label: "status_name"

    // Readiness.
    ready: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
}

impl Metrics {
    /// Create a new [`Metrics`] instance and register all metrics.
    pub fn new() -> Self {
        let registry = Registry::new();

        let up = register_gauge_vec_with_registry!(
            "linewatch_target_up",
            "Target reachable (1 = up, 0 = down)",
            &["target"],
            registry,
        )
        .unwrap();

        let loss = register_gauge_vec_with_registry!(
            "linewatch_target_loss_pct",
            "Packet loss percentage toward this target",
            &["target"],
            registry,
        )
        .unwrap();

        let rtt = register_histogram_vec_with_registry!(
            "linewatch_target_rtt_ms",
            "Round-trip time in milliseconds toward this target",
            &["target"],
            // Buckets from 1 ms to 10 s
            vec![
                1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 5000.0
            ],
            registry,
        )
        .unwrap();

        let temp = prometheus::register_gauge_with_registry!(
            "linewatch_temperature_c",
            "Current ambient temperature in °C",
            registry,
        )
        .unwrap();

        let outages = register_counter_vec_with_registry!(
            "linewatch_outages_total",
            "Outage events by category",
            &["category"],
            registry,
        )
        .unwrap();

        let status = register_gauge_vec_with_registry!(
            "linewatch_status",
            "Current status (0=Ok 1=Degraded 2=Down 3=LocalOrPower 4=DnsFail 5=HttpFail)",
            &["status_name"],
            registry,
        )
        .unwrap();

        Self {
            registry,
            up,
            loss,
            rtt,
            temp,
            outages,
            status,
            ready: Arc::new(AtomicBool::new(false)),
            healthy: Arc::new(AtomicBool::new(true)),
        }
    }

    // -- Per-cycle updates --------------------------------------------------

    /// Update per-target gauges from one probe outcome.
    pub fn update_target(&self, target: &str, outcome: &ProbeOutcome) {
        let v = if outcome.reachable { 1.0 } else { 0.0 };
        self.up.with_label_values(&[target]).set(v);
        self.loss
            .with_label_values(&[target])
            .set(f64::from(outcome.loss_pct));
        if let Some(rtt) = outcome.rtt {
            self.rtt
                .with_label_values(&[target])
                .observe(rtt.as_secs_f64() * 1000.0);
        }
    }

    /// Set the current temperature.
    pub fn update_temp(&self, temp_c: Option<f64>) {
        if let Some(t) = temp_c {
            self.temp.set(t);
        }
    }

    /// Increment the outage counter for a given category.
    pub fn record_outage(&self, category: &str) {
        self.outages.with_label_values(&[category]).inc();
    }

    /// Set the current status gauge — only one status_name label gets 1.0.
    pub fn update_status(&self, status: &Status) {
        // Reset all labels first, then set the current one.
        for name in &[
            "ok",
            "degraded",
            "down",
            "local_or_power",
            "dns_fail",
            "http_fail",
        ] {
            self.status.with_label_values(&[name]).set(0.0);
        }
        let (ord, name) = status_to_metric(status);
        self.status.with_label_values(&[name]).set(f64::from(ord));
    }

    // -- Server lifecycle ---------------------------------------------------

    /// Mark the service as ready (config + data dir validated).
    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Relaxed);
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    // -- HTTP handlers (called from axum) -----------------------------------

    async fn metrics_handler(self: &Arc<Self>) -> String {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        encoder
            .encode_to_string(&metric_families)
            .unwrap_or_default()
    }

    async fn healthz(self: &Arc<Self>) -> &'static str {
        if self.is_healthy() { "ok" } else { "unhealthy" }
    }

    async fn readyz(self: &Arc<Self>) -> &'static str {
        if self.is_ready() { "ok" } else { "not ready" }
    }

    // -- Server startup -----------------------------------------------------

    /// Bind to `addr` and serve the HTTP endpoints.
    pub async fn serve(self: Arc<Self>, addr: SocketAddr) {
        let app = Router::new()
            .route(
                "/metrics",
                get(|State(m): State<Arc<Self>>| async move { m.metrics_handler().await }),
            )
            .route(
                "/healthz",
                get(|State(m): State<Arc<Self>>| async move { m.healthz().await }),
            )
            .route(
                "/readyz",
                get(|State(m): State<Arc<Self>>| async move { m.readyz().await }),
            )
            .with_state(self);

        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn status_to_metric(s: &Status) -> (u8, &'static str) {
    match s {
        Status::Ok => (0, "ok"),
        Status::Degraded => (1, "degraded"),
        Status::Down => (2, "down"),
        Status::LocalOrPower => (3, "local_or_power"),
        Status::DnsFail => (4, "dns_fail"),
        Status::HttpFail => (5, "http_fail"),
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

    fn render(m: &Metrics) -> String {
        let encoder = TextEncoder::new();
        encoder.encode_to_string(&m.registry.gather()).unwrap()
    }

    #[test]
    fn metrics_update_and_render() {
        let m = Metrics::new();

        let outcome = ProbeOutcome {
            kind: TargetKind::Http,
            reachable: true,
            rtt: Some(Duration::from_millis(42)),
            loss_pct: 0,
        };
        m.update_target("https://example.com", &outcome);
        m.update_temp(Some(23.5));
        m.update_status(&Status::Ok);

        let buf = render(&m);
        assert!(buf.contains("linewatch_target_up{target=\"https://example.com\"} 1"));
        assert!(buf.contains("linewatch_temperature_c 23.5"));
        assert!(buf.contains("linewatch_status"));
        assert!(buf.contains("ok"));
        println!("{}", buf);
    }

    #[test]
    fn status_gauge_exclusive() {
        let m = Metrics::new();

        m.update_status(&Status::Down);
        let buf = render(&m);
        assert!(buf.contains("linewatch_status{status_name=\"down\"} 2"));
        assert!(buf.contains("linewatch_status{status_name=\"ok\"} 0"));

        // Switching to Ok resets Down.
        m.update_status(&Status::Ok);
        let buf = render(&m);
        assert!(buf.contains("linewatch_status{status_name=\"ok\"} 0"));
        assert!(buf.contains("linewatch_status{status_name=\"down\"} 0"));
    }

    #[test]
    fn outage_counter() {
        let m = Metrics::new();
        m.record_outage("complete_interruption");
        m.record_outage("complete_interruption");

        let buf = render(&m);
        assert!(buf.contains("linewatch_outages_total{category=\"complete_interruption\"} 2"));
    }

    #[tokio::test]
    async fn healthz_and_readyz() {
        let m = Arc::new(Metrics::new());

        // Before ready.
        assert_eq!(m.readyz().await, "not ready");
        assert_eq!(m.healthz().await, "ok");

        m.mark_ready();
        assert_eq!(m.readyz().await, "ok");
    }
}
