# linewatch

A Rust network-monitoring daemon. It pings things, keeps a tamper-proof
event log, and can spit out a report you'd feel OK sending to a customer.

Three output layers, intentionally kept separate:

1. **Prometheus / OTLP metrics** — live Grafana fodder.
2. **Append-only JSONL event log** with a SHA-256 hash chain. Once it's
   written, you can't quietly mess with it.
3. **Dossier** (Markdown or PDF) — a pure projection of the event log.
   Re-verifies the chain every time, never adds data that wasn't in the
   events.

Fully static binary (`x86_64-unknown-linux-musl`), rustls for TLS, no
OpenSSL, no shelling out to `ping`/`curl`/`traceroute`.

---

## Architecture: functional core / imperative shell

Everything that decides, classifies, or computes lives in `src/core/`.
Zero I/O, unit-tested. Everything that touches the network, filesystem, or
clock lives in `src/shell/`. The types they share (`Sample`, `Status`,
`OutageEvent`, `Record`, `Dossier`) are plain enums and structs.

States are enums. Logic is iterators and `match`. No surprises.

---

## What it actually does

Every `interval_secs` it runs all probes concurrently:

- **ICMP ping** to the default gateway (detected once at startup from
  `/proc/net/route`) and any configured ICMP anchors — `surge-ping`
- **TCP connect** to anchors — `tokio::TcpStream` with timeout
- **DNS resolution** — `hickory-resolver` over UDP
- **HTTP check** — `reqwest` with rustls
- **Temperature** — Open-Meteo API (cached for 5 min) or static value

When an outage closes, it runs a traceroute (ICMP-TTL with raw sockets,
falls back to TCP-TTL on port 443) and stashes the hops alongside the
event.

---

## Status classification

Strict priority, implemented as a single `core::classify::classify`
function:

1. Gateway unreachable → `LocalOrPower`
2. Every external anchor unreachable → `Down`
3. DNS failed → `DnsFail`
4. HTTP failed → `HttpFail`
5. Any probe exceeding `max_loss_pct` or `max_rtt_ms` → `Degraded`
6. Otherwise → `Ok`

`LocalOrPower` and `Ok` are transparent to the hysteresis machine — they
reset the bad counter instead of accumulating toward an outage.

---

## Hysteresis / debounce

`core::events::Machine` is a two-state machine (`Idle`, `Open`) driven by
`open_after` and `close_after`:

- A non-Ok, non-`LocalOrPower` sample increments `bad_count`. When it hits
  `open_after`, an event opens.
- An `Ok` sample increments `close_count`. When it hits `close_after`, the
  event closes and `Machine::advance` returns an `OutageEvent`.
- Any other status during an open event resets `close_count` and may
  escalate `worst_status` (ranking: `Down > DnsFail > HttpFail > Degraded`).
- `Machine::force_close` exists for graceful shutdown — emits whatever's
  in-flight with the current timestamp.

`OutageEvent` carries an `AgcomCategory`: `CompleteInterruption` if any
`Down` sample was seen, `IrregularService` otherwise.

---

## The hash chain

Every line in the JSONL log is a record with three chain fields:

```json
{
  "type": "sample" | "outage" | "monitor_restart",
  "seq": 42,
  "prev_hash": "<hex sha256 of record seq-1>",
  "hash":     "<hex sha256 of canonical_json(record_without_hash) + prev_hash>",
  ...
}
```

Canonical JSON (recursive key sort via `BTreeMap`) makes the hash
deterministic. `shell::store::StoreWriter` opens the log in append mode,
recovers the last `(seq, prev_hash)` from disk, writes a `MonitorRestart`
marker on open, and `fsync`s after every line. The hash is computed from
the body with the `hash` field zeroed, so the stored value can be
re-verified by `core::chain::verify_chain`.

A tampered line breaks the chain at its `seq`, and the dossier reports the
break instead of silently producing numbers from garbage.

---

## The dossier

`core::dossier::project` is one pure function that:

1. Verifies the hash chain.
2. Parses every line into a `Record`.
3. Computes from the events: `days_observed`, `outage_count`,
   `total_downtime`, `downtime_by_hour_band`, `temp_correlation`, and
   per-event indemnity math (`max(0, days - repair_window) × daily_rate`).

Two renderers wrap it without adding fields:

- `core::render_md::render_markdown` — Markdown to stdout with executive
  summary, event table, indemnity table, traceroute appendix, and a SHA-256
  document fingerprint footer.
- `shell::render_pdf::render_pdf` — paginated PDF via `genpdf`, optional
  chart PNG on the first page.
- `shell::chart::render_chart` — temperature line with outage spans using
  `plotters` and a bundled `assets/DejaVuSans.ttf` (no fontconfig needed
  on musl).

---

## Metrics

A dedicated axum server on `0.0.0.0:9980`:

| Route | What it's for |
|-------|---------------|
| `GET /metrics` | Prometheus text |
| `GET /healthz` | Liveness — 200 when the binary is up |
| `GET /readyz`  | Readiness — 200 after `mark_ready()` |

Instruments:

- `linewatch_target_up{target}` — 1/0 gauge
- `linewatch_target_loss_pct{target}` — gauge
- `linewatch_target_rtt_ms{target}` — histogram
- `linewatch_temperature_c` — gauge
- `linewatch_outages_total{category}` — counter
- `linewatch_status{status_name}` — gauge (mutually exclusive, see `status_to_metric`)

When `otlp_endpoint` is configured, `shell::otlp::OtlpExporter` mirrors the
same instruments to an OTLP/HTTP-protobuf collector every 10 s. If it's
unset the OTLP code path dead-eliminates at build time.

---

## Build & run

```bash
cargo build --release
./target/release/linewatch run
```

For a fully static musl build in Docker:

```bash
docker build -t linewatch .
docker run --rm -p 9980:9980 \
  -v /var/lib/linewatch:/var/lib/linewatch \
  linewatch run
```

The Dockerfile compiles against `rust:alpine`, strips the binary, runs as
non-root, grants `cap_net_raw+ep` (needed for raw-ICMP traceroute; the
unprivileged SOCK_DGRAM ping path works without it), and installs a default
`linewatch.toml` at `/etc/linewatch/linewatch.toml`.

If your kernel restricts `net.ipv4.ping_group_range`, either pass
`--cap-add=NET_RAW` to Docker or relax the sysctl:

```bash
sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"
```

---

## Configuration

TOML with environment overrides prefixed `LINEWATCH_`. Minimal example:

```toml
interval_secs = 5
data_dir = "/var/lib/linewatch"

[targets]
tcp_anchors  = ["1.1.1.1:443", "8.8.8.8:443"]
icmp_anchors = []
dns_upstream  = "1.1.1.1:53"
dns_query_name = "cloudflare.com"
http_url      = "https://www.google.com/generate_204"

[thresholds]
max_loss_pct = 10
max_rtt_ms   = 200

[debounce]
open_after  = 3
close_after = 3

[temp]
source = "open-meteo"
lat = 41.77
lon = 12.66
# static_c = 22.0   # uncomment for a fixed temperature

# otlp_endpoint = "http://localhost:4318/v1/metrics"
```

Environment example:

```bash
LINEWATCH_INTERVAL_SECS=10 \
LINEWATCH_TARGETS__HTTP_URL=https://example.com/health \
LINEWATCH_OTLP_ENDPOINT=http://collector:4318/v1/metrics \
linewatch run
```

---

## Commands

| Command | What it does |
|---------|--------------|
| `linewatch run` | Start the daemon. |
| `linewatch report --format md [--chart chart.png]` | Dump a Markdown dossier to stdout. |
| `linewatch report --format pdf [--chart chart.png]` | Write a PDF dossier. |

The `report` subcommand re-verifies the hash chain every time. If the
chain is broken, it says so in the *Chain Integrity* section and continues
— you get a report, but it tells you something's wrong.

---

## Testing

```bash
cargo test --bin linewatch
```

About 60-odd tests covering: hash-chain round-trip and tamper detection,
status classification precedence, hysteresis (blips, sustained down,
escalation, flapping, multiple events, min-temp tracking), dossier
projection on a multi-day fixture, indemnity repair-window zeroing,
broken-chain reporting, Markdown and PDF rendering, Prometheus metrics,
Open-Meteo caching, live network probes against public endpoints, gateway
detection, and traceroute to loopback / public / unreachable targets.

The live-network tests are best-effort — they print diagnostics and skip
assertions when the environment can't reach the targets.

---

## License

Apache-2.0. See `LICENSE`.
