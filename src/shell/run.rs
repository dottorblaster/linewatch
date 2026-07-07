//! Main monitoring loop — runs every `interval_secs`, probes targets,
//! classifies, feeds the hysteresis machine, updates metrics and the
//! hash-chain store, and handles graceful shutdown.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::future::join_all;
use time::OffsetDateTime;
use tokio::signal;
use tokio::sync::watch;

use crate::config::Config;
use crate::core::chain::{Record, RecordChain};
use crate::core::classify::classify;
use crate::core::events::{DebounceCfg as CoreDebounceCfg, Machine};
use crate::core::types::{ProbeOutcome, Sample, TargetKind, Thresholds};
use crate::shell::metrics::Metrics;
use crate::shell::otlp::OtlpExporter;
use crate::shell::probe::{default_gateway, dns_check, http_check, icmp_ping, tcp_connect};
use crate::shell::store::StoreWriter;
use crate::shell::temp::create_temperature_source;
use crate::shell::trace::trace;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub async fn run(config: Config) -> Result<()> {
    // --- Validate data directory ---
    tokio::fs::create_dir_all(&config.data_dir)
        .await
        .map_err(|e| anyhow::anyhow!("cannot create data dir {:?}: {e}", config.data_dir))?;

    // --- Open the hash-chain store ---
    let store_path = config.data_dir.join("events.jsonl");
    let mut store = StoreWriter::open(store_path)?;

    // --- Metrics (Prometheus + axum) ---
    let metrics = Arc::new(Metrics::new());
    metrics.mark_ready();

    // --- Optional OTLP exporter ---
    let otlp = OtlpExporter::new(config.otlp_endpoint.as_deref());

    // --- Spawn axum server in background ---
    let metrics_srv = metrics.clone();
    let metrics_addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 9980);
    let metrics_handle = tokio::spawn(async move {
        metrics_srv.serve(metrics_addr).await;
    });

    // --- Hysteresis machine ---
    let debounce = CoreDebounceCfg {
        open_after: config.debounce.open_after,
        close_after: config.debounce.close_after,
    };
    let mut machine = Machine::new();

    // --- Temperature source ---
    let temp_source = create_temperature_source(&config.temp);

    // --- Shutdown signal channel ---
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    // Spawn signal handlers (each gets its own clone of the sender).
    let sigint_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        signal::ctrl_c().await.ok();
        eprintln!("[linewatch] SIGINT received, shutting down...");
        let _ = sigint_tx.send(true);
    });

    #[cfg(unix)]
    {
        let sigterm_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
            sigterm.recv().await;
            eprintln!("[linewatch] SIGTERM received, shutting down...");
            let _ = sigterm_tx.send(true);
        });
    }

    // --- Detect default gateway (cache for the whole run) ---
    let gateway = default_gateway().await;
    if let Some(gw) = gateway {
        eprintln!("[linewatch] default gateway: {gw}");
    }

    // --- Main monitoring loop ---
    let interval = Duration::from_secs(config.interval_secs);
    let probe_timeout = Duration::from_secs(config.interval_secs.saturating_sub(1).max(1));

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => { /* proceed */ }
            _ = shutdown_rx.changed() => break,
        }

        let cycle_start = OffsetDateTime::now_utc();

        // --- Run all probes concurrently ---
        let mut outcomes = Vec::new();

        // Build probe futures.
        let mut probe_futs: Vec<tokio::task::JoinHandle<ProbeOutcome>> = Vec::new();

        // TCP anchors
        for addr in &config.targets.tcp_anchors {
            let addr = addr.clone();
            let dur = probe_timeout;
            probe_futs.push(tokio::spawn(async move {
                tcp_connect(TargetKind::TcpAnchor, &addr, dur).await
            }));
        }

        // ICMP anchors
        for addr in &config.targets.icmp_anchors {
            if let Ok(ip) = addr.parse::<IpAddr>() {
                let dur = probe_timeout;
                probe_futs.push(tokio::spawn(async move {
                    match ip {
                        IpAddr::V4(v4) => icmp_ping(TargetKind::IcmpAnchor, v4, dur).await,
                        IpAddr::V6(_) => ProbeOutcome {
                            kind: TargetKind::IcmpAnchor,
                            reachable: false,
                            rtt: None,
                            loss_pct: 100,
                        },
                    }
                }));
            }
        }

        // Gateway ICMP ping
        if let Some(gw) = gateway {
            let dur = probe_timeout;
            probe_futs.push(tokio::spawn(async move {
                icmp_ping(TargetKind::Gateway, gw, dur).await
            }));
        }

        // HTTP check
        let url = config.targets.http_url.clone();
        let dur = probe_timeout;
        probe_futs.push(tokio::spawn(async move {
            http_check(TargetKind::Http, &url, dur).await
        }));

        // DNS check
        let upstream = config.targets.dns_upstream.clone();
        let query = config.targets.dns_query_name.clone();
        let dur = probe_timeout;
        probe_futs.push(tokio::spawn(async move {
            dns_check(TargetKind::Dns, &upstream, &query, dur).await
        }));

        // --- Wait for all probe futures, then conditionally fetch temp ---
        for result in join_all(probe_futs).await {
            match result {
                Ok(outcome) => outcomes.push(outcome),
                Err(e) => {
                    eprintln!("[linewatch] probe task panicked: {e}");
                }
            }
        }

        // Only fetch temperature when at least one probe succeeded — pointless
        // (and misleading for the "temperature is too high" heuristic) when the
        // connection is already confirmed offline.
        let temp_c = if outcomes.iter().any(|o| o.reachable) {
            temp_source.read().await
        } else {
            None
        };

        // --- Build the Sample ---
        let sample = Sample {
            ts: cycle_start,
            temp_c,
            outcomes: outcomes.clone(),
        };

        // --- Classify ---
        let thresholds = Thresholds {
            max_loss_pct: config.thresholds.max_loss_pct,
            max_rtt_ms: config.thresholds.max_rtt_ms,
        };
        let status = classify(&sample, &thresholds);

        // --- Feed hysteresis machine ---
        let outage = machine.advance(cycle_start, status.clone(), temp_c, &debounce);

        // --- Update Prometheus metrics ---
        for outcome in &outcomes {
            let label = target_label(&outcome.kind);
            metrics.update_target(label, outcome);
        }
        metrics.update_temp(temp_c);
        metrics.update_status(&status);

        // --- Update OTLP metrics (if configured) ---
        if let Some(ref otlp) = otlp {
            for outcome in &outcomes {
                let label = target_label(&outcome.kind);
                otlp.record_target(label, outcome);
            }
            otlp.record_temp(temp_c);
            otlp.record_status(&status);
        }

        // --- Append Sample record to store ---
        let record = Record::Sample {
            chain: RecordChain {
                seq: 0,
                prev_hash: String::new(),
                hash: String::new(),
            },
            ts: cycle_start.to_string(),
            temp_c,
            outcomes: outcomes.clone(),
        };
        if let Err(e) = store.append_record(&record) {
            eprintln!("[linewatch] failed to append sample record: {e}");
        }

        // --- Handle outage event (if machine emitted one) ---
        if let Some(event) = outage {
            let trace_target = gateway.unwrap_or(Ipv4Addr::new(1, 1, 1, 1));
            let hops = trace(IpAddr::V4(trace_target), probe_timeout).await;

            eprintln!(
                "[linewatch] outage: {:?} — {:?} ({} hops)",
                event.category,
                event.worst_status,
                hops.len()
            );

            let outage_record = Record::Outage {
                chain: RecordChain {
                    seq: 0,
                    prev_hash: String::new(),
                    hash: String::new(),
                },
                event: event.clone(),
                hops: hops.clone(),
            };
            if let Err(e) = store.append_record(&outage_record) {
                eprintln!("[linewatch] failed to append outage record: {e}");
            }

            let cat = match &event.category {
                crate::core::events::AgcomCategory::CompleteInterruption => "complete_interruption",
                crate::core::events::AgcomCategory::IrregularService => "irregular_service",
            };
            metrics.record_outage(cat);

            if let Some(ref otlp) = otlp {
                otlp.record_outage(cat);
            }
        }
    }

    // --- Graceful shutdown ---
    eprintln!("[linewatch] shutting down...");

    // Force-close any open event.
    if let Some(event) = machine.force_close(OffsetDateTime::now_utc()) {
        let trace_target = gateway.unwrap_or(Ipv4Addr::new(1, 1, 1, 1));
        let hops = trace(IpAddr::V4(trace_target), probe_timeout).await;

        let outage_record = Record::Outage {
            chain: RecordChain {
                seq: 0,
                prev_hash: String::new(),
                hash: String::new(),
            },
            event: event.clone(),
            hops: hops.clone(),
        };
        if let Err(e) = store.append_record(&outage_record) {
            eprintln!("[linewatch] failed to append final outage record: {e}");
        }

        eprintln!("[linewatch] force-closed outage: {:?}", event.worst_status);
    }

    // Abort the metrics server.
    metrics_handle.abort();

    eprintln!("[linewatch] done.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn target_label(kind: &TargetKind) -> &'static str {
    match kind {
        TargetKind::Gateway => "gateway",
        TargetKind::IcmpAnchor => "icmp_anchor",
        TargetKind::TcpAnchor => "tcp_anchor",
        TargetKind::Dns => "dns",
        TargetKind::Http => "http",
    }
}
