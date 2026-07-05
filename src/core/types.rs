use std::time::Duration;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TargetKind {
    Gateway,
    IcmpAnchor,
    TcpAnchor,
    Dns,
    Http,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProbeOutcome {
    pub kind: TargetKind,
    pub reachable: bool,
    pub rtt: Option<Duration>,
    pub loss_pct: u8,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Sample {
    pub ts: time::OffsetDateTime,
    pub temp_c: Option<f64>,
    pub outcomes: Vec<ProbeOutcome>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Status {
    Ok,
    Degraded,
    Down,
    LocalOrPower,
    DnsFail,
    HttpFail,
}

#[derive(Debug, Clone)]
pub struct Thresholds {
    pub max_loss_pct: u8,
    pub max_rtt_ms: u64,
}
