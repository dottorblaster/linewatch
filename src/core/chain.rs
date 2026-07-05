//! Hash-chain record types and pure verification.
//!
//! Every event log line is a JSON [`Record`] with a sequence number, the
//! previous line's hash, and a SHA-256 hash chaining it to its predecessor.
//! [`verify_chain`] replays the chain and reports the first break.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Record types
// ---------------------------------------------------------------------------

/// Common chain metadata embedded in every record.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct RecordChain {
    pub seq: u64,
    pub prev_hash: String,
    pub hash: String,
}

/// A single entry in the append-only event log.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum Record {
    /// Periodic monitoring sample.
    #[serde(rename = "sample")]
    Sample {
        #[serde(flatten)]
        chain: RecordChain,
        ts: String,
        temp_c: Option<f64>,
        outcomes: Vec<crate::core::types::ProbeOutcome>,
    },

    /// Outage event (opened + closed).
    #[serde(rename = "outage")]
    Outage {
        #[serde(flatten)]
        chain: RecordChain,
        #[serde(flatten)]
        event: crate::core::events::OutageEvent,
        hops: Vec<crate::shell::trace::Hop>,
    },

    /// Monitor restart (startup marker).
    #[serde(rename = "monitor_restart")]
    MonitorRestart {
        #[serde(flatten)]
        chain: RecordChain,
    },
}

// ---------------------------------------------------------------------------
// Hash computation
// ---------------------------------------------------------------------------

/// Compute the chain hash for a record given its serialisable body and the
/// previous line's hash.
///
/// `hash = hex(sha256(canonical_json(record_without_hash) + prev_hash))`
pub fn compute_hash(body: &impl Serialize, prev_hash: &str) -> String {
    // 1. Serialise to Value so we can strip the "hash" field.
    let mut v = serde_json::to_value(body).expect("record must serialise to JSON");

    // 2. Remove the "hash" field — it is not part of the hash input.
    if let Value::Object(ref mut map) = v {
        map.remove("hash");
    }

    // 3. Canonical JSON: recursive key sorting.
    let canon = canonical_json(&v);
    let json_str = serde_json::to_string(&canon).expect("canonical JSON must serialise");

    // 4. Concatenate with prev_hash and hash.
    let mut hasher = Sha256::new();
    hasher.update(json_str.as_bytes());
    hasher.update(prev_hash.as_bytes());
    let result = hasher.finalize();

    hex::encode(result)
}

/// Recursively sort object keys for deterministic (canonical) JSON output.
fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonical_json(v)))
                .collect();
            Value::Object(serde_json::Map::from_iter(sorted))
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

// ---------------------------------------------------------------------------
// Chain verification
// ---------------------------------------------------------------------------

/// Result of [`verify_chain`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChainStatus {
    /// Whether every link in the chain is valid.
    pub intact: bool,
    /// The `seq` of the first record whose hash or prev_hash is wrong, or
    /// `None` when the chain is intact.
    pub break_at: Option<u64>,
}

/// Verify a sequence of deserialised JSON log lines.
///
/// Each line must be an object with `"seq"`, `"prev_hash"`, and `"hash"`
/// string/number fields.  Returns the first break or `intact: true`.
///
/// This is a **pure** function — no I/O, no mutation of the input.
pub fn verify_chain(lines: &[Value]) -> ChainStatus {
    for (i, line) in lines.iter().enumerate() {
        let seq = line.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
        let prev_hash = line.get("prev_hash").and_then(|v| v.as_str()).unwrap_or("");
        let hash = line.get("hash").and_then(|v| v.as_str()).unwrap_or("");

        // Check prev_hash chains to the previous line's hash.
        if i > 0 {
            let prev_line_hash = lines[i - 1]
                .get("hash")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if prev_hash != prev_line_hash {
                return ChainStatus {
                    intact: false,
                    break_at: Some(seq),
                };
            }
        }

        // Re-compute expected hash.
        let expected = compute_hash(line, prev_hash);
        if hash != expected {
            return ChainStatus {
                intact: false,
                break_at: Some(seq),
            };
        }
    }

    ChainStatus {
        intact: true,
        break_at: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Helper: write a record as a JSON line, return its Value.
    fn make_record(seq: u64, prev_hash: &str, body: &Value) -> Value {
        // Build a temporary RecordChain-like value so compute_hash can work.
        let mut v = body.clone();
        if let Value::Object(ref mut map) = v {
            map.insert("seq".into(), json!(seq));
            map.insert("prev_hash".into(), json!(prev_hash));
            // hash will be computed and inserted
        }
        let hash = compute_hash(&v, prev_hash);
        if let Value::Object(ref mut map) = v {
            map.insert("hash".into(), json!(hash));
        }
        v
    }

    #[test]
    fn chain_three_records_intact() {
        let h0 = "";
        let r0 = make_record(0, h0, &json!({"type": "monitor_restart"}));
        let h1 = r0["hash"].as_str().unwrap();
        let r1 = make_record(1, h1, &json!({"type": "monitor_restart"}));
        let h2 = r1["hash"].as_str().unwrap();
        let r2 = make_record(2, h2, &json!({"type": "monitor_restart"}));

        let lines = vec![r0, r1, r2];
        let status = verify_chain(&lines);
        assert!(status.intact, "chain should be intact");
        assert_eq!(status.break_at, None);
    }

    #[test]
    fn tampered_hash_detected() {
        let h0 = "";
        let mut r0 = make_record(0, h0, &json!({"type": "monitor_restart"}));
        let h1 = r0["hash"].as_str().unwrap();
        let r1 = make_record(1, h1, &json!({"type": "monitor_restart"}));

        // Tamper with record 0's hash.
        r0["hash"] = json!("deadbeef");

        let lines = vec![r0.clone(), r1];
        let status = verify_chain(&lines);
        assert!(!status.intact, "tampered chain should be broken");
        assert_eq!(status.break_at, Some(0));
    }

    #[test]
    fn tampered_prev_hash_detected() {
        let h0 = "";
        let r0 = make_record(0, h0, &json!({"type": "monitor_restart"}));
        let h1 = r0["hash"].as_str().unwrap();
        let mut r1 = make_record(1, h1, &json!({"type": "monitor_restart"}));

        // Tamper with record 1's prev_hash.
        r1["prev_hash"] = json!("0000");

        let lines = vec![r0, r1];
        let status = verify_chain(&lines);
        assert!(!status.intact);
        assert_eq!(status.break_at, Some(1));
    }

    #[test]
    fn tampered_field_in_middle_detected() {
        let h0 = "";
        let r0 = make_record(0, h0, &json!({"type": "monitor_restart"}));
        let h1 = r0["hash"].as_str().unwrap();
        let r1 = make_record(1, h1, &json!({"type": "monitor_restart"}));
        let h2 = r1["hash"].as_str().unwrap();
        let r2 = make_record(2, h2, &json!({"type": "monitor_restart"}));

        let mut lines = vec![r0, r1, r2];
        // Tamper with record 1's seq.
        lines[1]["seq"] = json!(99u64);

        let status = verify_chain(&lines);
        assert!(!status.intact);
        // The break is at seq=99 (the tampered record).
        assert_eq!(status.break_at, Some(99));
    }

    #[test]
    fn sample_record_roundtrip_and_verify() {
        use crate::core::types::{ProbeOutcome, TargetKind};
        use std::time::Duration;

        let outcome = ProbeOutcome {
            kind: TargetKind::Http,
            reachable: true,
            rtt: Some(Duration::from_millis(42)),
            loss_pct: 0,
        };

        let record = Record::Sample {
            chain: RecordChain {
                seq: 0,
                prev_hash: "".into(),
                hash: "".into(), // filled below
            },
            ts: "2026-07-05T12:00:00Z".into(),
            temp_c: Some(22.5),
            outcomes: vec![outcome],
        };

        // Compute and set the hash.
        let hash = compute_hash(&record, "");
        let record = Record::Sample {
            chain: RecordChain {
                seq: 0,
                prev_hash: "".into(),
                hash: hash.clone(),
            },
            ts: "2026-07-05T12:00:00Z".into(),
            temp_c: Some(22.5),
            outcomes: vec![ProbeOutcome {
                kind: TargetKind::Http,
                reachable: true,
                rtt: Some(Duration::from_millis(42)),
                loss_pct: 0,
            }],
        };

        // Serialise and verify.
        let v = serde_json::to_value(&record).unwrap();
        // The serialised value must contain the computed hash.
        assert_eq!(v["hash"].as_str(), Some(hash.as_str()));

        let lines = vec![v];
        let status = verify_chain(&lines);
        assert!(status.intact);
    }
}
