//! The durable local buffer, and the source of truth.
//!
//! Every handler writes here and returns; nothing sends at emit time. Offline is
//! the normal path, not an error path. Line-oriented JSONL gives crash-safety
//! for free: a process killed mid-append leaves one torn last line, which
//! ingest skips and counts as `bad_lines`.
//!
//! Ported from `compound/spool.py` with the same file naming, the same rotation
//! rule and the same overflow behaviour, because the uploader and the server
//! both already work with those and there is nothing wrong with them.

#[cfg(test)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::envelope::{self, Event};

pub struct Spool {
    dir: PathBuf,
    max_bytes: u64,
    debug: bool,
    uid: String,
    did: String,
}

impl Spool {
    pub fn new(cfg: &Config) -> Self {
        Spool {
            dir: cfg.spool_dir.clone(),
            max_bytes: cfg.spool_max_bytes,
            debug: cfg.debug,
            uid: cfg.uid.clone(),
            did: cfg.did.clone(),
        }
    }

    fn current(&self) -> PathBuf {
        self.dir.join("current.jsonl")
    }

    pub fn append(&self, e: &Event) {
        if let Err(err) = self.try_append(e) {
            // A spool write failing is the one error worth being loud about:
            // it is the difference between "offline" and "losing data".
            eprintln!("unvelt: spool append failed: {err}");
            return;
        }
        if self.debug {
            eprintln!(
                "emit {}/{} {}",
                e.src,
                e.et,
                e.p.as_ref()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "".into())
            );
        }
    }

    fn try_append(&self, e: &Event) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.current())?;
        f.write_all(envelope::dumps(e).as_bytes())?;
        f.write_all(b"\n")
    }

    /// A `meta.gap`, so "absent evidence" and "no evidence" stay distinct.
    pub fn gap(&self, cfg: &Config, src: &str, from_ms: i64, to_ms: i64, why: &str) {
        let e = envelope::build(
            cfg,
            "meta",
            "gap",
            to_ms,
            format!("gap:{src}:{why}:{from_ms}:{to_ms}"),
            Some(serde_json::json!({"src": src, "from": from_ms, "to": to_ms, "why": why})),
        );
        self.append(&e);
    }

    pub fn rotate(&self) {
        let cur = self.current();
        let Ok(meta) = fs::metadata(&cur) else { return };
        if meta.len() == 0 {
            return;
        }
        let name = format!("{}-{}.jsonl", crate::now_ms(), std::process::id());
        let _ = fs::rename(&cur, self.dir.join(name));
    }

    /// Rotated files ready to upload, oldest first.
    ///
    /// Sorted by name rather than mtime, which works because the name starts
    /// with a millisecond timestamp and ordering is what preserves the
    /// server-side event order on a slow link.
    pub fn ready(&self) -> Vec<PathBuf> {
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = rd
            .flatten()
            .map(|d| d.path())
            .filter(|p| {
                p.extension().map(|e| e == "jsonl").unwrap_or(false)
                    && p.file_name().map(|n| n != "current.jsonl").unwrap_or(false)
            })
            .collect();
        out.sort();
        out
    }

    pub fn stats(&self) -> (usize, u64) {
        let ready = self.ready();
        let bytes = ready.iter().filter_map(|p| size_of(p)).sum();
        (ready.len(), bytes)
    }

    /// Drop the oldest spooled batches once the buffer outgrows its cap, and
    /// say so in the data rather than only in a log nobody reads.
    pub fn enforce_cap(&self, cfg: &Config) {
        let mut ready = self.ready();
        let mut total: u64 = ready.iter().filter_map(|p| size_of(p)).sum();
        while total > self.max_bytes && !ready.is_empty() {
            let victim = ready.remove(0);
            let Some(sz) = size_of(&victim) else { continue };
            if fs::remove_file(&victim).is_err() {
                break;
            }
            total -= sz;
            let name = victim
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let now = crate::now_ms();
            let e = envelope::build(
                cfg,
                "meta",
                "gap",
                now,
                format!("gap:spool:overflow:{name}"),
                Some(serde_json::json!({"src": "spool", "why": "spool_overflow", "dropped": name})),
            );
            self.append(&e);
        }
    }

    /// Only used by the self-check, but it needs the same identity the events
    /// carry or a stray spool directory from another account looks like ours.
    pub fn describe(&self) -> String {
        format!("{} (uid={} did={})", self.dir.display(), self.uid, self.did)
    }
}

fn size_of(p: &Path) -> Option<u64> {
    fs::metadata(p).ok().map(|m| m.len())
}

/// Read a spool file back as parsed lines.
///
/// Test-only: the parity diff that matters runs against the database, where
/// both collectors' events land, not against one machine's spool files.
#[cfg(test)]
pub fn read_lines(p: &Path) -> std::io::Result<Vec<serde_json::Value>> {
    use std::io::{BufRead, BufReader};
    let f = File::open(p)?;
    Ok(BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cfg() -> Config {
        // A millisecond stamp alone is not unique: cargo runs these tests in
        // parallel threads of one process, and two of them started in the same
        // millisecond shared a spool directory, so one test counted the other
        // test's files. Pid plus a counter makes it actually unique.
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let mut c = Config::for_test();
        c.spool_dir = std::env::temp_dir().join(format!(
            "unvelt-test-{}-{}-{}",
            crate::now_ms(),
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        c.spool_max_bytes = 200;
        c
    }

    #[test]
    fn append_then_rotate_makes_a_ready_file() {
        let cfg = temp_cfg();
        let s = Spool::new(&cfg);
        let e = envelope::build(&cfg, "desktop", "active", 1, "a".into(), None);
        s.append(&e);
        assert!(s.ready().is_empty(), "unrotated work is not ready to send");
        s.rotate();
        assert_eq!(s.ready().len(), 1);
        let rows = read_lines(&s.ready()[0]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["et"], "active");
        let _ = fs::remove_dir_all(&cfg.spool_dir);
    }

    #[test]
    fn overflow_drops_the_oldest_and_records_a_gap() {
        let cfg = temp_cfg();
        let s = Spool::new(&cfg);
        for i in 0..6 {
            let e = envelope::build(&cfg, "desktop", "active", i, format!("e{i}"), None);
            s.append(&e);
            s.rotate();
            // Names carry a millisecond stamp; without this two rotations in
            // the same millisecond would collide and the test would be
            // measuring the clock rather than the cap.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        s.enforce_cap(&cfg);
        let (_n, bytes) = s.stats();
        assert!(
            bytes <= cfg.spool_max_bytes,
            "cap not enforced: {bytes} > {}",
            cfg.spool_max_bytes
        );
        let all: Vec<_> = s
            .ready()
            .iter()
            .flat_map(|p| read_lines(p).unwrap_or_default())
            .chain(read_lines(&cfg.spool_dir.join("current.jsonl")).unwrap_or_default())
            .collect();
        assert!(
            all.iter()
                .any(|v| v["et"] == "gap" && v["p"]["why"] == "spool_overflow"),
            "dropping data must leave a meta.gap behind"
        );
        let _ = fs::remove_dir_all(&cfg.spool_dir);
    }
}
