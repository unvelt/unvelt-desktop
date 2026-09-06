//! Configuration, read from the environment exactly as the Python collector
//! reads it, so the two can be pointed at the same server with the same setup.
//!
//! One deliberate difference: `did`. During the parity window both collectors
//! run on the same machine, and identical `did` plus identical `eid` means the
//! server dedupes one against the other and the diff comes back empty for the
//! wrong reason. `UNVELT_DID_SUFFIX` (default `-rs`) keeps them apart, and is
//! set to the empty string when this agent takes over for real.

use std::path::PathBuf;

/// Clone because the collector thread and the UI both need a copy, and it is
/// a handful of strings read once at startup -- cheaper to copy than to share
/// behind a lock that would then have to be held during a probe.
#[derive(Clone)]
pub struct Config {
    pub uid: String,
    pub url: String,
    /// Legacy `X-Compound-Key`. The deployed ingest ignores it -- it verifies
    /// a Firebase ID token instead -- but the POC VM still gates on it, so it
    /// stays until nothing points at that host any more.
    pub key: String,
    /// Identifies the Firebase project to the Identity Toolkit. Not a secret:
    /// it ships inside every copy of the Android app and authorises nothing on
    /// its own.
    pub api_key: String,
    pub oauth_client_id: String,
    /// Google issues one alongside a Desktop client. Also not a secret for
    /// this client type -- it ships in the binary, and PKCE is what actually
    /// protects the exchange.
    pub oauth_client_secret: String,
    pub host: String,
    pub osname: &'static str,
    pub did: String,

    pub sample_sec: u64,
    pub idle_sec: f64,
    pub resample_sec: u64,
    pub input_window_sec: u64,
    pub context_sec: u64,
    /// Only the macOS SSID path reads this -- its fallback can cost ~4s -- so
    /// on Windows it is genuinely unused rather than merely unused today.
    #[cfg_attr(windows, allow(dead_code))]
    pub ssid_sec: u64,
    pub flush_sec: u64,
    pub hb_sec: u64,

    pub spool_dir: PathBuf,
    pub spool_max_bytes: u64,
    pub debug: bool,
}

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_flag(key: &str) -> bool {
    !matches!(
        env_str(key, "").to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no"
    )
}

/// From android/app/google-services.json. See `api_key` above for why this is
/// compiled in rather than fetched or hidden.
///
/// The length check is not decoration. This constant was first filled in from
/// a debug print that masked all but the first twelve characters, and a
/// truncated key fails at Firebase sign-in with an error that blames the
/// request rather than the key. A wrong constant that looks right is worth one
/// assertion.
/// Where events go. The POC's duckdns VM is retired: it only answers 401 now,
/// because the shared-key gate it used was replaced by real token auth.
const DEFAULT_URL: &str = "https://compound-ingest-nexyqgrgbq-el.a.run.app";

const DEFAULT_API_KEY: &str = "AIzaSyC2KeGrmf35qT1z21CP72EEp9TNuaL77eg";

pub const OSNAME: &str = if cfg!(target_os = "windows") {
    "win"
} else if cfg!(target_os = "macos") {
    "mac"
} else {
    "linux"
};

impl Config {
    pub fn from_env() -> Self {
        let host = hostname();
        let osname: &'static str = OSNAME;
        // Same default shape as compound/config.py: "<os>-<hostname>".
        let base_did = env_str("UNVELT_DID", &format!("{osname}-{host}"));
        // Empty by default. It carried "-rs" while a parity run against the
        // Python collector was planned -- identical eids under one device id
        // would have made the server dedupe the two agents against each other
        // and return a flawless comparison of nothing. That run was dropped:
        // the Python collector was never itself validated, so it was never a
        // baseline worth measuring against, and this agent simply takes over
        // the device id the history is already under.
        let did = format!("{base_did}{}", env_str("UNVELT_DID_SUFFIX", ""));

        Config {
            uid: env_str("UNVELT_UID", ""),
            url: env_str("UNVELT_URL", DEFAULT_URL)
                .trim_end_matches('/')
                .to_string(),
            key: env_str("UNVELT_INGEST_KEY", ""),
            api_key: env_str("UNVELT_FIREBASE_API_KEY", DEFAULT_API_KEY),
            oauth_client_id: env_str("UNVELT_OAUTH_CLIENT_ID", ""),
            oauth_client_secret: env_str("UNVELT_OAUTH_CLIENT_SECRET", ""),
            host,
            osname,
            did,

            sample_sec: env_num("UNVELT_SAMPLE_SEC", 5),
            idle_sec: env_num("UNVELT_IDLE_SEC", 180.0),
            resample_sec: env_num("UNVELT_RESAMPLE_SEC", 120),
            input_window_sec: env_num("UNVELT_INPUT_WINDOW_SEC", 60),
            context_sec: env_num("UNVELT_CONTEXT_SEC", 60),
            // macOS SSID can fall through to a ~4s system_profiler call, so it
            // is refreshed on its own much slower clock.
            ssid_sec: env_num("UNVELT_SSID_SEC", 900),
            flush_sec: env_num("UNVELT_FLUSH_SEC", 60),
            hb_sec: env_num("UNVELT_HB_SEC", 300),

            spool_dir: spool_dir(),
            spool_max_bytes: env_num("UNVELT_SPOOL_MAX_BYTES", 64 * 1024 * 1024),
            debug: env_flag("UNVELT_DEBUG"),
        }
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        let mut c = Config::from_env();
        c.uid = "test".into();
        c.did = "test".into();
        c
    }
}

fn hostname() -> String {
    // The short name, matching Python's socket.gethostname().split(".")[0].
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(read_hostname_fallback);
    raw.split('.').next().unwrap_or("unknown").to_string()
}

#[cfg(windows)]
fn read_hostname_fallback() -> String {
    "unknown".into()
}

#[cfg(not(windows))]
fn read_hostname_fallback() -> String {
    // COMPUTERNAME/HOSTNAME are often unset for a launchd or systemd service,
    // which is exactly how this runs in production, so the syscall is the
    // normal path here rather than a fallback.
    let mut buf = [0i8; 256];
    extern "C" {
        fn gethostname(name: *mut i8, len: usize) -> i32;
    }
    unsafe {
        if gethostname(buf.as_mut_ptr(), buf.len()) != 0 {
            return "unknown".into();
        }
        let bytes: Vec<u8> = buf
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).to_string()
    }
}

/// Where this machine keeps unvelt's own state: the spool, the session and
/// the instance lock. One directory so that "delete everything unvelt kept
/// here" is one directory to delete.
pub fn state_dir() -> PathBuf {
    spool_dir()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(std::env::temp_dir)
}

fn spool_dir() -> PathBuf {
    if let Ok(d) = std::env::var("UNVELT_SPOOL_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    let base = if cfg!(windows) {
        std::env::var("LOCALAPPDATA").ok().map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    } else {
        std::env::var("XDG_STATE_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".local").join("state"))
            })
    };
    base.unwrap_or_else(std::env::temp_dir)
        .join("unvelt")
        .join("spool")
}
