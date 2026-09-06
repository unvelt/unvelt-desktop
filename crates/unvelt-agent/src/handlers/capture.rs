//! Whether the camera or microphone is live — the meetings signal.
//!
//! This exists because of a false negative that nothing else can fix. Someone
//! in a two-hour call types almost nothing, so `desktop.input` reports a
//! near-zero ratio, `desk_present` sees no activity, and every presence rule
//! concludes they were away from the machine. Two hours of concentrated work
//! reads as two hours of absence. This is the only signal that can say
//! otherwise.
//!
//! WHERE IT COMES FROM ON WINDOWS
//!
//! `HKCU\...\CapabilityAccessManager\ConsentStore\{webcam,microphone}` is the
//! store behind the Settings page that shows which apps recently used your
//! camera. Each app has `LastUsedTimeStart` and `LastUsedTimeStop` as FILETIME
//! values, and **an app is using the device right now exactly when it has a
//! start and no stop**. Reading it needs no permission and no elevation.
//!
//! It is not a private API in any meaningful sense -- it is the same data the
//! Settings app renders -- but it is also not documented as a contract, so the
//! reader treats a missing or malformed key as "no signal" rather than as
//! false.
//!
//! WHAT IT DELIBERATELY DOES NOT DO
//!
//! It never touches the camera or the microphone. It reads a registry key that
//! records what other applications did. Nothing here can see or hear anything,
//! and no OS permission prompt appears, because none is required to read it --
//! which is precisely why the switch in the app defaults to off.

use std::collections::BTreeSet;

use super::{Handler, Tick};
use crate::config::Config;
use crate::envelope::Event;

/// (device, app) pairs currently in use.
type InUse = BTreeSet<(&'static str, String)>;

pub struct CaptureHandler {
    interval: f64,
    last: Option<InUse>,
    warned: bool,
}

impl CaptureHandler {
    pub fn new(cfg: &Config) -> Self {
        CaptureHandler {
            // A call starting is worth catching within a few seconds, but this
            // is a registry walk rather than a single value read, so it runs
            // on the sample clock rather than every tick.
            interval: (cfg.sample_sec as f64).max(10.0),
            last: None,
            warned: false,
        }
    }
}

impl Handler for CaptureHandler {
    fn name(&self) -> &'static str {
        "capture"
    }

    fn interval(&self) -> f64 {
        self.interval
    }

    fn poll(&mut self, tick: &mut Tick) -> Vec<Event> {
        let now = match in_use() {
            Ok(s) => s,
            Err(e) => {
                if tick.cfg.debug && !self.warned {
                    self.warned = true;
                    eprintln!("unvelt: camera/mic state unavailable: {e}");
                }
                return Vec::new();
            }
        };
        let prev = match self.last.replace(now.clone()) {
            // The first reading seeds the baseline without emitting. Starting
            // the agent while a call is already running is not the call
            // beginning, and an event for it would put a phantom edge at every
            // process start -- the same rule the lock handler follows.
            None => return Vec::new(),
            Some(p) => p,
        };

        let mut out = Vec::new();
        for (dev, app) in now.difference(&prev) {
            out.push(edge(tick, dev, app, 1));
        }
        for (dev, app) in prev.difference(&now) {
            out.push(edge(tick, dev, app, 0));
        }
        out
    }
}

fn edge(tick: &Tick, dev: &str, app: &str, on: u8) -> Event {
    let mut p = serde_json::Map::new();
    p.insert("dev".into(), serde_json::json!(dev));
    p.insert("on".into(), serde_json::json!(on));
    // Omitted rather than empty when the platform will not say who holds the
    // device -- CoreMediaIO reports that a camera is running and nothing
    // about which app. An empty string would read as an app with no name.
    if !app.is_empty() {
        p.insert("app".into(), serde_json::json!(app));
    }
    tick.event(
        "desktop",
        "capture",
        tick.now,
        // Keyed on the transition, not just the time: camera and microphone
        // change independently and can move in the same tick, so a
        // timestamp-only eid would collide two real edges into one.
        format!("dt:{}:cap:{dev}:{on}:{}", tick.cfg.did, tick.now),
        Some(serde_json::Value::Object(p)),
    )
}

#[cfg(windows)]
fn in_use() -> Result<InUse, String> {
    let mut out = InUse::new();
    for (dev, key) in [("cam", "webcam"), ("mic", "microphone")] {
        let base = format!(
            "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\{key}"
        );
        for app in reg::apps_in_use(&base)? {
            out.insert((dev, app));
        }
    }
    Ok(out)
}

/// macOS, from Core Audio and CoreMediaIO. Both public, neither prompts.
///
/// The asymmetry with Windows is real and shows in the data: Core Audio names
/// the process holding the microphone, CoreMediaIO exposes only whether a
/// camera is running and nothing about who is running it. So `cam` arrives
/// with no `app` here. The payload omits the field rather than inventing a
/// value, because "we do not know which app" and "some app called ''" are
/// different facts.
#[cfg(target_os = "macos")]
fn in_use() -> Result<InUse, String> {
    let mut out = InUse::new();
    for app in crate::backend::mac_av::recording_apps() {
        out.insert(("mic", app));
    }
    match crate::backend::mac_av::camera_running() {
        // An empty app is the marker for "running, holder unknown". `edge`
        // omits the field when it sees this.
        Some(true) => {
            out.insert(("cam", String::new()));
        }
        Some(false) => {}
        // CoreMediaIO would not answer. Saying nothing is right: "no camera is
        // on" and "we cannot see cameras" are different claims.
        None => {}
    }
    Ok(out)
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn in_use() -> Result<InUse, String> {
    // Linux would mean watching /dev/video* and PulseAudio separately, which
    // is its own change.
    Err("camera and microphone state is not implemented on this platform".into())
}

#[cfg(windows)]
mod reg {
    use std::collections::BTreeSet;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn open(parent: HKEY, path: &str) -> Option<HKEY> {
        let mut h: HKEY = std::ptr::null_mut();
        let ok = unsafe { RegOpenKeyExW(parent, wide(path).as_ptr(), 0, KEY_READ, &mut h) };
        (ok == ERROR_SUCCESS).then_some(h)
    }

    fn subkeys(h: HKEY) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0u32;
        loop {
            let mut buf = [0u16; 512];
            let mut len = buf.len() as u32;
            let r = unsafe {
                RegEnumKeyExW(
                    h,
                    i,
                    buf.as_mut_ptr(),
                    &mut len,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if r != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
            i += 1;
        }
        out
    }

    fn qword(h: HKEY, name: &str) -> Option<u64> {
        let mut data = [0u8; 8];
        let mut len = data.len() as u32;
        let mut kind = 0u32;
        let r = unsafe {
            RegQueryValueExW(
                h,
                wide(name).as_ptr(),
                std::ptr::null(),
                &mut kind,
                data.as_mut_ptr(),
                &mut len,
            )
        };
        (r == ERROR_SUCCESS && len == 8).then(|| u64::from_le_bytes(data))
    }

    /// An app is using the device when it has a start time and no stop time.
    /// A key with neither is one that used the device at some point in the
    /// past and is not using it now.
    fn is_open(h: HKEY) -> bool {
        let start = qword(h, "LastUsedTimeStart").unwrap_or(0);
        let stop = qword(h, "LastUsedTimeStop").unwrap_or(0);
        start > 0 && stop == 0
    }

    /// The app key as `desktop.focus` would name it, so one app has one
    /// identifier across both. Packaged apps are already AUMIDs; non-packaged
    /// ones are the executable path with `\` written as `#`, and reduce to the
    /// same base name `focus` sends.
    fn app_name(raw: &str) -> String {
        if !raw.contains('#') {
            return raw.to_string();
        }
        let path = raw.replace('#', "\\");
        let file = path.rsplit('\\').next().unwrap_or(&path).to_string();
        match file.rfind('.') {
            Some(i) => file[..i].to_string(),
            None => file,
        }
    }

    pub fn apps_in_use(base: &str) -> Result<BTreeSet<String>, String> {
        let Some(root) = open(HKEY_CURRENT_USER, base) else {
            // No consent store for this device means nothing has ever asked
            // for it on this machine. Not an error.
            return Ok(BTreeSet::new());
        };
        let mut found = BTreeSet::new();
        for name in subkeys(root) {
            // "NonPackaged" is a container, not an app: its children are the
            // desktop programs. Everything else at this level is a packaged
            // app keyed by AUMID.
            if name.eq_ignore_ascii_case("NonPackaged") {
                if let Some(np) = open(root, &name) {
                    for child in subkeys(np) {
                        if let Some(k) = open(np, &child) {
                            if is_open(k) {
                                found.insert(app_name(&child));
                            }
                            unsafe { RegCloseKey(k) };
                        }
                    }
                    unsafe { RegCloseKey(np) };
                }
                continue;
            }
            if let Some(k) = open(root, &name) {
                if is_open(k) {
                    found.insert(app_name(&name));
                }
                unsafe { RegCloseKey(k) };
            }
        }
        unsafe { RegCloseKey(root) };
        Ok(found)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn non_packaged_keys_reduce_to_the_name_focus_uses() {
            // `desktop.focus` sends the executable base name, so this must
            // produce the same string or one app arrives under two keys and
            // `app_labels` can never fold them.
            assert_eq!(
                super::app_name("C:#Program Files#Zoom#bin#Zoom.exe"),
                "Zoom"
            );
            assert_eq!(super::app_name("C:#Windows#System32#Teams.exe"), "Teams");
            // Packaged apps are AUMIDs already and must be left alone.
            assert_eq!(
                super::app_name("Microsoft.Windows.Photos_8wekyb3d8bbwe"),
                "Microsoft.Windows.Photos_8wekyb3d8bbwe"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_the_real_store_does_not_fail_or_lie() {
        // Not a mock. The value of this handler rests entirely on a registry
        // layout Microsoft does not document as a contract, so the thing worth
        // asserting is that a real machine answers without erroring and
        // without claiming something implausible.
        match in_use() {
            Ok(set) => {
                assert!(set.len() < 64, "implausible number of capture users");
                for (dev, app) in &set {
                    assert!(*dev == "cam" || *dev == "mic");
                    assert!(!app.is_empty(), "an app in use with no name");
                }
            }
            // `cfg!` is a constant, so clippy rightly refuses to let it be
            // asserted on. The intent survives as a branch: on Windows there
            // is no acceptable error, and everywhere else an error IS the
            // expected answer.
            Err(e) => {
                #[cfg(windows)]
                panic!("Windows should always be able to read the consent store: {e}");
                #[cfg(not(windows))]
                assert!(!e.is_empty(), "a refusal should say why");
            }
        }
    }
}
