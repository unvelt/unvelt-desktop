//! The collector, as a library.
//!
//! Two things consume this: the `unvelt-agent` binary next to it, which is a
//! CLI and nothing else, and the Tauri app in `unvelt-app`, which runs the
//! same loop on a background thread behind a tray icon.
//!
//! It is a library rather than a process the app supervises, and that is a
//! deliberate call. A sidecar would mean two binaries to sign, two to update,
//! an IPC channel to define, and a supervision problem — what the app should
//! do when the collector dies, what the collector should do when the app is
//! closed — for a component that is a five-second polling loop. Embedding it
//! leaves one process, one updater, one signature, and the window free to be
//! destroyed and rebuilt without touching collection, which is the whole
//! reason for choosing Tauri over Electron in the first place.
//!
//! The window is not where the work happens. Closing it must never stop the
//! loop, so nothing in these modules knows a window exists.

pub mod auth;
pub mod backend;
pub mod client;
pub mod config;
pub mod controller;
pub mod envelope;
pub mod handlers;
pub mod spool;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Everything the UI is allowed to know about the collector's state.
///
/// Deliberately a snapshot rather than a handle to the controller. A status
/// window that could reach into the running loop would eventually be given a
/// button that changes it mid-cycle, and the loop's correctness rests on
/// nobody doing that.
#[derive(serde::Serialize, Clone, Debug)]
pub struct Status {
    pub version: &'static str,
    pub device_id: String,
    pub host: String,
    pub platform: &'static str,
    pub signed_in: bool,
    pub endpoint: String,
    pub spool_files: usize,
    pub spool_bytes: u64,
    /// `None` until the loop has completed a cycle.
    pub last_tick_ms: Option<i64>,
    pub last_flush_ms: Option<i64>,
    pub events_spooled: u64,
    pub collecting: bool,
}

/// What one probe of this machine currently answers.
///
/// The UI shows this verbatim, including the blanks. A signal the OS will not
/// give us is a fact about the machine worth seeing, not something to hide
/// behind a zero — the same rule the handlers follow when they stay quiet.
#[derive(serde::Serialize, Clone, Debug, Default)]
pub struct Probe {
    pub app: Option<String>,
    pub title: Option<String>,
    pub idle_s: f64,
    pub fullscreen: Option<bool>,
    pub locked: Option<bool>,
    pub monitors: Option<i32>,
    pub ssid: Option<String>,
    pub net: Option<String>,
    pub ac: Option<bool>,
    pub battery_pct: Option<u8>,
}

pub fn probe_once(cfg: &config::Config) -> Probe {
    let mut b = backend::make_backend(cfg);
    let front = b.frontmost();
    let power = b.power();
    Probe {
        app: front.as_ref().map(|f| f.pkg.clone()),
        title: front
            .as_ref()
            .map(|f| f.title.clone())
            .filter(|t| !t.is_empty()),
        idle_s: b.idle(),
        fullscreen: b.fullscreen(),
        locked: b.locked(),
        monitors: b.monitors(),
        ssid: b.ssid(),
        net: b.net(),
        ac: power.as_ref().map(|p| p.ac),
        battery_pct: power.as_ref().and_then(|p| p.pct),
    }
}
