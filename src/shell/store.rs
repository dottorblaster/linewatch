//! Append-only hash-chain log writer.
//!
//! [`StoreWriter`] opens a JSONL file in append mode, seeds the chain from
//! the last existing line, writes a `MonitorRestart` marker on construction,
//! and appends one record per call with an `fsync` after each write.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use crate::core::chain::{Record, RecordChain, compute_hash};

// ---------------------------------------------------------------------------
// StoreWriter
// ---------------------------------------------------------------------------

/// Append-only event log with an internal SHA-256 hash chain.
pub struct StoreWriter {
    file: File,
    seq: u64,
    prev_hash: String,
}

impl StoreWriter {
    /// Open (or create) the log at `path`, read the last line to recover the
    /// chain state, and append a `MonitorRestart` record.
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;

        // Recover chain state from the last existing line.
        let (seq, prev_hash) = read_last_line(&path)?;

        let mut writer = StoreWriter {
            file,
            seq,
            prev_hash,
        };

        // Append a MonitorRestart record.
        let restart = Record::MonitorRestart {
            chain: RecordChain {
                seq: writer.seq,
                prev_hash: writer.prev_hash.clone(),
                hash: String::new(), // filled by append_record
            },
        };
        writer.append_record(&restart)?;

        Ok(writer)
    }

    /// Serialise `record`, compute its chain hash, write it as a JSON line,
    /// and `fsync` the file.
    pub fn append_record(&mut self, record: &Record) -> std::io::Result<()> {
        // Build final record with the correct seq + prev_hash first, so that
        // the hash is computed over the actual data that gets stored.
        let chain = RecordChain {
            seq: self.seq,
            prev_hash: self.prev_hash.clone(),
            hash: String::new(), // placeholder
        };
        let mut record = replace_chain(record, chain);

        let hash = compute_hash(&record, &self.prev_hash);

        // Inject the computed hash.
        set_hash(&mut record, &hash);

        let line = serde_json::to_string(&record)
            .map_err(std::io::Error::other)?;

        writeln!(self.file, "{}", line)?;
        self.file.sync_all()?;

        self.seq += 1;
        self.prev_hash = hash;

        Ok(())
    }

    /// Current sequence number (next record's `seq`).
    #[allow(dead_code)]
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Set the `hash` field inside a Record's chain.
fn set_hash(record: &mut Record, hash: &str) {
    let chain = match record {
        Record::Sample { chain, .. }
        | Record::Outage { chain, .. }
        | Record::MonitorRestart { chain } => chain,
    };
    chain.hash = hash.to_owned();
}

/// Replace the `chain` field in a Record with a new RecordChain.
fn replace_chain(record: &Record, chain: RecordChain) -> Record {
    match record {
        Record::Sample {
            chain: _,
            ts,
            temp_c,
            outcomes,
        } => Record::Sample {
            chain,
            ts: ts.clone(),
            temp_c: *temp_c,
            outcomes: outcomes.clone(),
        },
        Record::Outage {
            chain: _,
            event,
            hops,
        } => Record::Outage {
            chain,
            event: event.clone(),
            hops: hops.clone(),
        },
        Record::MonitorRestart { chain: _ } => Record::MonitorRestart { chain },
    }
}

/// Read the last line from the event log and return `(next_seq, prev_hash)`.
///
/// For an empty / absent file this returns `(0, "")`.
fn read_last_line(path: &PathBuf) -> std::io::Result<(u64, String)> {
    // Use a separate flag so we can distinguish "empty file" from "file with
    // seq=0".
    let file = match File::open(path) {
        Ok(f) => f,
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((0, String::new()));
        }
        Err(e) => return Err(e),
    };

    let reader = BufReader::new(file);
    let mut last_seq: u64 = 0;
    let mut last_hash: String = String::new();
    let mut found_any = false;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(seq) = val.get("seq").and_then(|v| v.as_u64()) {
                last_seq = seq;
                found_any = true;
            }
            if let Some(h) = val.get("hash").and_then(|v| v.as_str()) {
                last_hash = h.to_owned();
            }
        }
    }

    if found_any {
        Ok((last_seq + 1, last_hash))
    } else {
        Ok((0, String::new()))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::chain::verify_chain;
    use std::io::Read;

    #[test]
    fn append_then_verify_intact() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("linewatch_test_{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut store = StoreWriter::open(path.clone()).unwrap();
        let initial_seq = store.seq();

        // Append a few monitor_restart records.
        for _ in 0..3 {
            let rec = Record::MonitorRestart {
                chain: RecordChain {
                    seq: 0, // will be replaced
                    prev_hash: String::new(),
                    hash: String::new(),
                },
            };
            store.append_record(&rec).unwrap();
        }

        assert_eq!(store.seq(), initial_seq + 3);

        // Read back and verify.
        let mut contents = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        let lines: Vec<serde_json::Value> = contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        let status = verify_chain(&lines);
        assert!(status.intact, "chain should be intact: {:?}", status);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tamper_detected_on_reload() {
        let dir = std::env::temp_dir();
        let unique = format!(
            "linewatch_tamper_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let path = dir.join(unique);

        {
            let mut store = StoreWriter::open(path.clone()).unwrap();
            let rec = Record::MonitorRestart {
                chain: RecordChain {
                    seq: 0,
                    prev_hash: String::new(),
                    hash: String::new(),
                },
            };
            store.append_record(&rec).unwrap();
        }

        // Tamper with the file on disk.
        let mut contents = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        let tampered = contents.replacen("monitor_restart", "xoinitor_restart", 1);
        std::fs::write(&path, tampered).unwrap();

        // Read back and verify.
        let mut contents = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        let lines: Vec<serde_json::Value> = contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        assert_eq!(lines.len(), 2, "expected exactly 2 lines in file");

        let status = verify_chain(&lines);
        assert!(!status.intact, "tampered chain should break");
        assert_eq!(status.break_at, Some(0));

        std::fs::remove_file(&path).ok();
    }
}
