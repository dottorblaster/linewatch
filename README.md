# linewatch

A small, self-contained Rust network-monitoring daemon that produces three
**separate, non-overlapping** output layers:

1. **Prometheus / OTLP metrics** for live Grafana viewing.
2. An **append-only JSONL event log** with an internal **SHA-256 hash chain** —
   the immutable source of truth.
3. A **dossier** (Markdown or PDF), a **deterministic pure projection** of
   the JSONL that re-verifies the chain before emitting and never adds data
   not derivable from the events.

The binary is fully static (`x86_64-unknown-linux-musl`), uses **rustls** for
TLS (no OpenSSL), and never shells out to `ping`/`curl`/`traceroute` —
everything is pure-Rust.

---

## Architecture: functional core / imperative shell

| Layer | Location | Rules |
|------|----------|-------|
| **Core** (pure) | `src/core/` | All classification, hysteresis, event coalescing, indemnity math, dossier projection, and chain verification. **No I/O**, unit-tested. |
| **Shell** (effectful) | `src/shell/` | Sockets, files, clock, signals, HTTP server, Open-Meteo, ICMP raw sockets, OTLP, PDF rendering. |

States are modeled as `enum`s, logic is iterator- and `match`-based. The
core types (`Sample`, `Status`, `OutageEvent`, `Record`, `Dossier`, …) are
the only data structures shared across the boundary.

---

## What it monitors

On every `interval_secs` cycle the daemon runs **all probes concurrently**:

| Probe | Module | Backend |
|-------|--------|---------|
| Default-gateway ICMP | `shell::probe::icmp_ping` | `surge-ping` (SOCK_DGRAM, raw fallback) |
| ICMP anchors | `shell::probe::icmp_ping` | `surge-ping` |
| TCP anchors | `shell::probe::tcp_connect` | `tokio::net::TcpStream` + `tokio::time::timeout` |
| DNS | `shell::probe::dns_check` | `hickory-resolver` (UDP) |
| HTTP | `shell::probe::http_check` | `reqwest` + **rustls** |
| Temperature | `shell::temp` | Open-Meteo API (5-min in-memory cache) or static |

The default gateway is detected once at start-up by reading
`/proc/net/route` (no `ip route` shell-out).

When an outage event closes, the daemon runs a **traceroute** with
`shell::trace::trace` (ICMP-TTL with raw-socket CAP_NET_RAW, falling back
to TCP-TTL on `target:443`) and stores the hops alongside the outage.

---

## Status classification

Implemented in `core::classify::classify` with a **strict precedence**:

1. Default gateway unreachable → `LocalOrPower`
2. All external anchors (ICMP + TCP) unreachable → `Down`
3. DNS probe failed → `DnsFail`
4. HTTP probe failed → `HttpFail`
5. Any reachable probe exceeds `max_loss_pct` or `max_rtt_ms` → `Degraded`
6. Otherwise → `Ok`

`LocalOrPower` and `Ok` are *transparent* to the hysteresis machine: they
reset the bad counter instead of accumulating toward an outage.

---

## Hysteresis / debounce

`core::events::Machine` is a two-state machine (`Idle`, `Open`) driven by
`open_after` and `close_after` from the config:

- A non-Ok/non-`LocalOrPower` sample increments `bad_count`. When it
  reaches `open_after`, the streak opens an event and `Machine::advance`
  records the `worst_status`, `min_temp_c`, and `samples_count`.
- An `Ok` sample increments `close_count`. When it reaches `close_after`
  the event **closes** and the function returns an `OutageEvent`.
- Any other status during an open event **resets** `close_count` and may
  escalate `worst_status`. Severity ranking: `Down > DnsFail > HttpFail > Degraded`.
- `Machine::force_close` is called on graceful shutdown to emit any
  in-flight event with the current timestamp.

`OutageEvent` carries an `AgcomCategory`:

- `CompleteInterruption` — at least one `Down` sample was seen
- `IrregularService` — only `Degraded` / `DnsFail` / `HttpFail`

---

## The hash chain

Every record on disk is a JSON object with three chain fields:

```json
{
  "type": "sample" | "outage" | "monitor_restart",
  "seq": 42,
  "prev_hash": "<hex sha256 of record seq-1>",
  "hash":     "<hex sha256 of canonical_json(record_without_hash) + prev_hash>",
  ...
}
```

Canonical-JSON normalisation (recursive key sort via `BTreeMap`) makes the
hash deterministic regardless of map insertion order.

`shell::store::StoreWriter`:

- Opens the log in **append** mode
- Recovers the last `(seq, prev_hash)` from the last line on disk
- Writes a `MonitorRestart` marker immediately on open
- `fsync`s after every line
- Recomputes the hash from the *body* (with `hash` field zeroed) so the
  stored value can be re-verified by `core::chain::verify_chain`

A tampered or missing line breaks the chain at the lowest affected `seq`
and `dossier.project` reports the break in `chain_status`.

---

## The dossier

`core::dossier::project` is a **single pure function** that:

1. Calls `verify_chain` on the input.
2. Parses every line into a `Record`.
3. Computes, from the events alone:
   - `days_observed` (first → last timestamp)
   - `outage_count`, `total_downtime`
   - `downtime_by_hour_band` — `[00–06, 06–12, 12–18, 18–24]`
   - `temp_correlation` — downtime within a daytime window, and what share
     of that downtime occurred above a temperature threshold
   - `per_event_indemnities` — one line per closed event with a
     human-readable `formula = max(0, days − repair_window) × daily_rate`

Two renderers wrap it without adding fields:

- `core::render_md::render_markdown` — Markdown with executive summary,
  chronological event table, indemnity table, traceroute appendix, and a
  SHA-256 document fingerprint footer.
- `shell::render_pdf::render_pdf` — paginated PDF via `genpdf`, embedding
  the optional chart (PNG) on the first page.
- `shell::chart::render_chart` — temperature line + shaded outage spans
  using `plotters` and the bundled `assets/DejaVuSans.ttf` (so `musl` does
  not need fontconfig).

---

## Observability endpoints

`shell::metrics::Metrics` registers the following on a dedicated axum
server bound to `0.0.0.0:9980`:

| Route | Purpose |
|-------|---------|
| `GET /metrics` | Prometheus text exposition |
| `GET /healthz` | Liveness — always 200 once the binary is up |
| `GET /readyz`  | Readiness — 200 after `mark_ready()` |

Metrics exposed:

- `linewatch_target_up{target}` — 1/0 gauge
- `linewatch_target_loss_pct{target}` — gauge
- `linewatch_target_rtt_ms{target}` — histogram
- `linewatch_temperature_c` — gauge
- `linewatch_outages_total{category}` — counter
- `linewatch_status{status_name}` — gauge (mutually exclusive, see `status_to_metric`)

When `otlp_endpoint` is set, `shell::otlp::OtlpExporter` mirrors the same
instruments to an OTLP/HTTP-protobuf collector every 10 s using
`opentelemetry-otlp` with the `reqwest-rustls` feature only. When unset,
no OTLP code path runs at all.

---

## Build & run

### Cargo (host build)

```bash
cargo build --release
./target/release/linewatch run
```

### Static musl + `FROM scratch`-friendly image

```bash
docker build -t linewatch .
docker run --rm -p 9980:9980 \
  -v /var/lib/linewatch:/var/lib/linewatch \
  linewatch run
```

The Dockerfile:

- Compiles with `rust:alpine` for `x86_64-unknown-linux-musl`
- Strips the binary
- Runs as a non-root user `linewatch` (uid 1000)
- Grants `cap_net_raw+ep` on the binary (needed for raw-ICMP fallback in
  the traceroute; the unprivileged SOCK_DGRAM path needs no caps)
- Exposes `9980` for the metrics endpoints
- Installs a default `linewatch.toml` at `/etc/linewatch/linewatch.toml`

If your kernel restricts `net.ipv4.ping_group_range`, you can either pass
`--cap-add=NET_RAW` or relax the sysctl before running:

```bash
sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"
```

---

## Configuration

Loaded via `figment` from `linewatch.toml` with environment overrides
prefixed `LINEWATCH_`. Example (`linewatch.toml`):

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
source = "open-meteo"   # or "static"
lat = 41.77
lon = 12.66
# static_c = 22.0

# Optional OTLP endpoint. When unset, no OTLP code runs.
# otlp_endpoint = "http://localhost:4318/v1/metrics"
```

Equivalent env-var override example:

```bash
LINEWATCH_INTERVAL_SECS=10 \
LINEWATCH_TARGETS__HTTP_URL=https://example.com/health \
LINEWATCH_OTLP_ENDPOINT=http://collector:4318/v1/metrics \
./target/release/linewatch run
```

---

## Commands

| Command | Purpose |
|---------|---------|
| `linewatch run` | Start the monitoring daemon (the container default). |
| `linewatch report --format md [--chart chart.png]` | Project `events.jsonl` into a Markdown dossier on stdout. |
| `linewatch report --format pdf [--chart chart.png]` | Project into a PDF written to `<data_dir>/dossier.pdf`. |

The `report` subcommand always re-verifies the hash chain before computing
anything, so a broken chain is reported in the dossier's *Chain Integrity*
section and never silently produces numbers from tampered data.

---

## Testing

```bash
cargo test --bin linewatch
```

58 tests across the workspace cover:

- Hash-chain round-trip, tamper detection on hash, `prev_hash`, and
  middle-record fields (`core::chain`, `shell::store`)
- Status classification precedence (`core::classify`)
- Hysteresis: single blips, sustained down, escalation, flapping,
  multiple events, min-temperature tracking (`core::events`)
- Dossier projection on a multi-day fixture, indemnity repair-window
  zeroing, broken-chain reporting (`core::dossier`)
- Markdown and PDF rendering on the same fixture (`core::render_md`,
  `shell::render_pdf`, `shell::chart`)
- Prometheus metric updates, exclusive status gauge, outage counter
  (`shell::metrics`)
- Open-Meteo caching, static fallback, factory routing (`shell::temp`)
- Live network probes against public endpoints (TCP/HTTP/DNS/ICMP),
  `default_gateway` reading, traceroute to loopback / public /
  unreachable targets (`shell::probe`, `shell::trace`)

The live network tests are best-effort: they print diagnostics and skip
assertions when the environment cannot reach the targets.

---

## Project layout

```
linewatch/
├── AGENTS.md              # development rules (architecture, constraints)
├── Cargo.toml             # deps — all added with `cargo add`
├── Dockerfile             # musl static build, alpine runtime
├── LICENSE                # Apache-2.0
├── linewatch.toml         # default config
├── assets/
│   └── DejaVuSans.ttf     # bundled font, embedded with include_bytes!
└── src/
    ├── main.rs            # clap entry, dispatches run / report
    ├── cli.rs             # Command enum
    ├── config.rs          # figment TOML+env loader
    ├── core/              # ── pure, no I/O ──
    │   ├── mod.rs
    │   ├── types.rs       # Sample, ProbeOutcome, Status, TargetKind, Thresholds
    │   ├── chain.rs       # Record, RecordChain, compute_hash, verify_chain
    │   ├── classify.rs    # pure status classification
    │   ├── events.rs      # Machine (hysteresis), OutageEvent, AgcomCategory
    │   ├── dossier.rs     # project() and indemnity math
    │   └── render_md.rs   # Markdown dossier renderer
    └── shell/             # ── I/O, sockets, files, clock ──
        ├── mod.rs
        ├── run.rs         # main loop, signal handling, graceful shutdown
        ├── probe.rs       # TCP / ICMP / DNS / HTTP / gateway
        ├── trace.rs       # ICMP-TTL traceroute w/ TCP fallback
        ├── temp.rs        # Open-Meteo + Static temperature source
        ├── store.rs       # append-only hash-chain JSONL writer
        ├── metrics.rs     # Prometheus registry + axum server
        ├── otlp.rs        # optional OpenTelemetry OTLP exporter
        ├── chart.rs       # PNG temperature vs outage timeline
        └── render_pdf.rs  # PDF dossier via genpdf
```

---

## License

Apache-2.0. See `LICENSE`.
