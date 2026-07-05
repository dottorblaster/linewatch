use figment::providers::{Env, Format, Toml};
use figment::Figment;
use serde::Deserialize;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Target configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TargetsCfg {
    pub tcp_anchors: Vec<String>,
    pub icmp_anchors: Vec<String>,
    pub dns_upstream: String,
    pub dns_query_name: String,
    pub http_url: String,
}

// ---------------------------------------------------------------------------
// Temperature configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TempCfg {
    pub source: String,
    pub lat: f64,
    pub lon: f64,
    pub static_c: Option<f64>,
}

// ---------------------------------------------------------------------------
// Thresholds and debounce (config-level mirrors of core types, with
// Deserialize for TOML loading)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Thresholds {
    pub max_loss_pct: u8,
    pub max_rtt_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DebounceCfg {
    pub open_after: u32,
    pub close_after: u32,
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Config {
    pub interval_secs: u64,
    pub data_dir: PathBuf,
    pub targets: TargetsCfg,
    pub thresholds: Thresholds,
    pub debounce: DebounceCfg,
    pub temp: TempCfg,
}

impl Config {
    pub fn load() -> Result<Self, figment::Error> {
        Figment::new()
            .merge(Toml::file("linewatch.toml"))
            .merge(Env::prefixed("LINEWATCH_").ignore(&["_".into()]))
            .extract()
    }
}
