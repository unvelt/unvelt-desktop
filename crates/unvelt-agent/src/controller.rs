//! The durable heart of the collector.
//!
//! Owns the loop clock and the shared per-cycle snapshot; reads the cheap
//! global signals once (idle, fullscreen) and hands each handler a `Tick`;
//! spools whatever they emit; flushes on a cadence; heartbeats; and — because
//! it owns the clock — it is the thing that can notice the machine slept. A
//! wall-clock jump between cycles becomes an explicit `meta.gap`, so a blind
//! window is never silent.
//!
//! Durability: the spool is the source of truth and survives a crash,
//! deterministic eids make retries free, and Ctrl-C flushes before exiting.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::backend::Backend;
use crate::client::ApiClient;
use crate::config::Config;
use crate::envelope;
use crate::handlers::{Handler, Tick};
use crate::sources::Toggles;
use crate::spool::Spool;
use crate::{Status, VERSION};

pub struct Controller {
    cfg: Config,
    client: ApiClient,
    spool: Spool,
    handlers: Vec<Box<dyn Handler>>,
    backend: Box<dyn Backend>,
    last_run: Vec<i64>,
    stop: Arc<AtomicBool>,
    /// Set while the user has paused collection from the tray. Distinct from
    /// `stop`: paused keeps the process, the spool and the session alive and
    /// simply stops asking the OS anything, so resuming costs nothing and the
    /// gap is one the person chose.
    paused: Arc<AtomicBool>,
    /// What the UI is allowed to see. A snapshot published each cycle rather
    /// than a handle to this struct, so no window can reach into a running
    /// loop -- see the note on `Status`.
    state: Arc<Mutex<Status>>,
    /// The local per-device switches. Not the consent ledger -- see
    /// `sources.rs` for why those are different questions.
    toggles: Toggles,
}

impl Controller {
    pub fn new(
        cfg: Config,
        client: ApiClient,
        spool: Spool,
        handlers: Vec<Box<dyn Handler>>,
        backend: Box<dyn Backend>,
    ) -> Self {
        let last_run = vec![0; handlers.len()];
        let (files, bytes) = spool.stats();
        let state = Status {
            version: VERSION,
            device_id: cfg.did.clone(),
            host: cfg.host.clone(),
            platform: cfg.osname,
            signed_in: client.signed_in(),
            endpoint: cfg.url.clone(),
            spool_files: files,
            spool_bytes: bytes,
            last_tick_ms: None,
            last_flush_ms: None,
            events_spooled: 0,
            watching_since_ms: None,
            collecting: true,
            duplicate: false,
            email: None,
            uid: None,
        };
        Controller {
            cfg,
            client,
            spool,
            handlers,
            backend,
            last_run,
            stop: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(state)),
            toggles: Toggles::load(),
        }
    }

    pub fn toggles(&self) -> Toggles {
        self.toggles.clone()
    }

    pub fn status_handle(&self) -> Arc<Mutex<Status>> {
        self.state.clone()
    }

    pub fn pause_flag(&self) -> Arc<AtomicBool> {
        self.paused.clone()
    }

    /// Publish the current snapshot. Never blocks the loop on a poisoned lock:
    /// a status window that stops updating is a nuisance, a collector that
    /// stops collecting is the failure this whole design is built to avoid.
    fn publish(&self, f: impl FnOnce(&mut Status)) {
        if let Ok(mut st) = self.state.lock() {
            f(&mut st);
        }
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    /// One cycle of the loop. Split out so `--once` can exercise the whole
    /// path -- probes, handlers, spool -- without a long-running process,
    /// which is what makes the parity harness runnable in CI.
    pub fn tick_once(&mut self) {
        let now = crate::now_ms();
        if self.paused.load(Ordering::Relaxed) {
            self.publish(|st| {
                st.collecting = false;
                // Cleared, not kept: resuming starts a new stretch of
                // watching, and carrying the old start time across a pause
                // would claim coverage for minutes nobody was looking.
                st.watching_since_ms = None;
                st.last_tick_ms = Some(now);
            });
            return;
        }
        let idle = self.backend.idle();
        let fs = self.backend.fullscreen().unwrap_or(false);
        // Fullscreen means gaming or a film: present, not away, however long
        // it has been since the last keypress.
        let away = idle >= self.cfg.idle_sec && !fs;

        let signed_in = self.client.signed_in();
        let mut spooled = 0u64;
        for i in 0..self.handlers.len() {
            let interval_ms = (self.handlers[i].interval() * 1000.0) as i64;
            if now - self.last_run[i] < interval_ms {
                continue;
            }
            // A switch the person set on this machine. Checked here rather
            // than by removing the handler, so turning it back on costs
            // nothing and the handler keeps whatever state it had.
            if !self.toggles.is_enabled(self.handlers[i].name()) {
                continue;
            }
            self.last_run[i] = now;
            let events = {
                let mut tick = Tick {
                    cfg: &self.cfg,
                    now,
                    idle,
                    away,
                    backend: self.backend.as_mut(),
                };
                self.handlers[i].poll(&mut tick)
            };
            if self.cfg.debug && !events.is_empty() {
                eprintln!(
                    "tick {} -> {} event(s) from {}",
                    now,
                    events.len(),
                    self.handlers[i].name()
                );
            }
            for e in &events {
                self.spool.append(e);
            }
            spooled += events.len() as u64;
        }

        let (files, bytes) = self.spool.stats();
        self.publish(|st| {
            st.collecting = true;
            st.watching_since_ms.get_or_insert(now);
            st.last_tick_ms = Some(now);
            st.events_spooled += spooled;
            st.spool_files = files;
            st.spool_bytes = bytes;
            st.signed_in = signed_in;
        });
    }

    pub fn run(&mut self) {
        let now = crate::now_ms();
        let start = envelope::build(
            &self.cfg,
            "meta",
            "start",
            now,
            format!("meta:start:{}:{}", self.cfg.did, now),
            Some(serde_json::json!({
                "ver": VERSION,
                "os": self.cfg.osname,
                "host": self.cfg.host,
            })),
        );
        self.spool.append(&start);

        let base = self.cfg.sample_sec;
        // Three missed ticks, floored at 30s. Below that a slow probe on a
        // loaded machine would report itself as a sleep.
        let gap_ms = std::cmp::max(30_000, base as i64 * 3 * 1000);
        let mut last_tick = now;
        let mut last_flush = now;
        let mut last_hb: i64 = 0;

        while !self.stop.load(Ordering::Relaxed) {
            let now = crate::now_ms();
            if now - last_tick > gap_ms {
                // The clock jumped: the machine was asleep or off.
                //
                // Two records, because they answer different questions. The
                // `meta.gap` tells coverage we were not looking, which keeps a
                // dark stretch from reading as "nothing happened". The
                // sleep/wake pair tells the digest WHY, which is a fact about
                // the person's day rather than about the collector -- a closed
                // lid is a finding, a dead process is a defect, and absence
                // alone cannot tell them apart.
                //
                // Derived from the clock rather than from a power event, and
                // that is deliberate: this works identically on all three
                // platforms and cannot miss a wake that the OS forgot to
                // announce. The cost is that it cannot distinguish a lid from
                // a shutdown, so `why` says exactly what was observed.
                self.spool
                    .gap(&self.cfg, "desktop", last_tick, now, "sleep_or_off");
                let asleep = (now - last_tick) / 1000;
                for (et, ts, extra) in [
                    ("sleep", last_tick, serde_json::json!({"why": "clock_jump"})),
                    (
                        "wake",
                        now,
                        serde_json::json!({"why": "clock_jump", "after_s": asleep}),
                    ),
                ] {
                    let e = envelope::build(
                        &self.cfg,
                        "desktop",
                        et,
                        ts,
                        format!("dt:{}:{et}:{ts}", self.cfg.did),
                        Some(extra),
                    );
                    self.spool.append(&e);
                }
            }
            last_tick = now;

            self.tick_once();

            if now - last_flush >= self.cfg.flush_sec as i64 * 1000 {
                self.flush();
                last_flush = now;
            }
            if now - last_hb >= self.cfg.hb_sec as i64 * 1000 {
                self.heartbeat();
                last_hb = now;
            }
            self.interruptible_sleep(base);
        }
        self.flush();
    }

    pub fn flush(&self) {
        self.spool.rotate();
        let started = crate::now_ms();
        for p in self.spool.ready() {
            let Ok(body) = std::fs::read(&p) else {
                continue;
            };
            if self.client.post_events(&body) {
                let _ = std::fs::remove_file(&p);
            } else {
                // Keep order: a later batch must not overtake an earlier one,
                // so a failure stops the drain rather than skipping past it.
                break;
            }
        }
        self.spool.enforce_cap(&self.cfg);
        let (files, bytes) = self.spool.stats();
        self.publish(|st| {
            st.last_flush_ms = Some(started);
            st.spool_files = files;
            st.spool_bytes = bytes;
        });
    }

    pub fn heartbeat(&self) {
        let (nfiles, nbytes) = self.spool.stats();
        let hb = serde_json::json!({
            "uid": self.cfg.uid,
            "did": self.cfg.did,
            "ts": crate::now_ms(),
            "ver": VERSION,
            "spool_files": nfiles,
            "spool_bytes": nbytes,
            "srcs": ["desktop"],
            "dev": {
                "model": self.cfg.host,
                // `mfr` is how ingest tells a laptop from a phone: platform_of()
                // reads it first and exactly, so it must stay one of
                // win | mac | linux.
                "mfr": self.cfg.osname,
                "rel": os_release(),
            },
        });
        self.client.post_heartbeat(&hb);
    }

    fn interruptible_sleep(&self, seconds: u64) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        while !self.stop.load(Ordering::Relaxed) && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

/// A human-readable OS version, the equivalent of Python's platform.platform().
/// Ingest stores it as `core.devices.os_version` and falls back to reading its
/// prefix when the heartbeat carries no `mfr`, so the leading word matters.
fn os_release() -> String {
    #[cfg(windows)]
    {
        let out = crate::backend::run("cmd", &["/c", "ver"]);
        let t = out.trim();
        if !t.is_empty() {
            return t.to_string();
        }
        "Windows".to_string()
    }
    #[cfg(target_os = "macos")]
    {
        let v = crate::backend::run("sw_vers", &["-productVersion"]);
        format!("macOS-{}", v.trim())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let v = crate::backend::run("uname", &["-r"]);
        format!("Linux-{}", v.trim())
    }
}
