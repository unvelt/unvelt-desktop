//! Two facts about the room rather than the machine: what the sound is coming
//! out of, and whether the person asked not to be interrupted.
//!
//! WHY DO NOT DISTURB MATTERS MORE THAN IT LOOKS
//!
//! Without it, an evening with no notifications is indistinguishable from an
//! evening where somebody asked for none. That is precisely the on/off/gap
//! distinction coverage spends its whole design preserving, one level up: at
//! the operating system rather than at the collector. A quiet hour is a
//! finding; a silenced hour is a decision, and reading the second as the first
//! would make "you were not interrupted" the answer to a question nobody asked.
//!
//! WHY THE AUDIO ROUTE
//!
//! Headphones are a different kind of listening from a speaker in a shared
//! room, and it is the cheapest proxy there is for whether somebody is alone.
//! Only the route is taken, never what is playing -- that is `media`'s job and
//! it has its own switch.

use super::{Handler, Tick};
use crate::config::Config;
use crate::envelope::Event;

pub struct AmbientHandler {
    interval: f64,
    last_route: Option<(String, Option<String>)>,
    last_dnd: Option<String>,
    last_devices: Option<Vec<String>>,
}

impl AmbientHandler {
    pub fn new(cfg: &Config) -> Self {
        AmbientHandler {
            interval: cfg.context_sec as f64,
            last_route: None,
            last_dnd: None,
            last_devices: None,
        }
    }
}

impl Handler for AmbientHandler {
    fn name(&self) -> &'static str {
        "ambient"
    }

    fn interval(&self) -> f64 {
        self.interval
    }

    fn poll(&mut self, tick: &mut Tick) -> Vec<Event> {
        let mut out = Vec::new();

        if let Some((route, name)) = output_route() {
            let now = (route.to_string(), name.clone());
            if self.last_route.as_ref() != Some(&now) {
                // Seeding still emits here, unlike the edge-based handlers.
                // A route is a state, not a transition: knowing you were on
                // headphones from the moment the agent started is the useful
                // fact, and there is no phantom edge to invent because nothing
                // is claimed to have *changed*.
                self.last_route = Some(now);
                let mut p = serde_json::Map::new();
                p.insert("out".into(), serde_json::json!(route));
                // Device names are frequently a person's own name ("Kanishak's
                // AirPods"), which is why the registry marks this field full
                // depth. Sent, but never invented when absent.
                if let Some(n) = name {
                    p.insert("name".into(), serde_json::json!(n));
                }
                out.push(tick.event(
                    "desktop",
                    "audio",
                    tick.now,
                    format!("dt:{}:audio:{}", tick.cfg.did, tick.now),
                    Some(serde_json::Value::Object(p)),
                ));
            }
        }

        // Peripherals, in the form that is actually useful: an audio device
        // appearing is a headset going on or a dock being connected, and both
        // mean somebody sat down at a desk. Deliberately NOT a full USB
        // enumeration -- that reports a hundred things nobody cares about,
        // almost none of which ever change.
        let devices = playback_devices();
        if !devices.is_empty() {
            if let Some(prev) = self.last_devices.replace(devices.clone()) {
                for name in devices.iter().filter(|d| !prev.contains(d)) {
                    out.push(peripheral(tick, name, 1));
                }
                for name in prev.iter().filter(|d| !devices.contains(d)) {
                    out.push(peripheral(tick, name, 0));
                }
            }
            // First reading seeds: a headset already plugged in when the agent
            // started was not plugged in then.
        }

        if let Some((on, mode)) = dnd_state() {
            // Keyed on the pair, not just `on`: moving from a fullscreen game
            // to an explicit Do Not Disturb is a real change even though
            // notifications were suppressed throughout.
            if self.last_dnd.as_deref() != Some(mode) {
                self.last_dnd = Some(mode.to_string());
                out.push(tick.event(
                    "desktop",
                    "dnd",
                    tick.now,
                    format!("dt:{}:dnd:{mode}:{}", tick.cfg.did, tick.now),
                    Some(serde_json::json!({ "on": on as u8, "mode": mode })),
                ));
            }
        }

        out
    }
}

fn peripheral(tick: &Tick, name: &str, on: u8) -> Event {
    tick.event(
        "desktop",
        "peripheral",
        tick.now,
        format!("dt:{}:periph:{name}:{on}:{}", tick.cfg.did, tick.now),
        // `kind` is always audio here, and saying so is better than leaving a
        // required field to be inferred from the name.
        Some(serde_json::json!({ "kind": "audio", "name": name, "on": on })),
    )
}

#[cfg(windows)]
fn playback_devices() -> Vec<String> {
    crate::backend::win_audio::playback_devices()
}

#[cfg(not(windows))]
fn playback_devices() -> Vec<String> {
    // macOS would use the same Core Audio device list `mac_av` already talks
    // to; not written yet.
    Vec::new()
}

#[cfg(target_os = "macos")]
fn output_route() -> Option<(&'static str, Option<String>)> {
    crate::backend::mac_av::output_route()
}

#[cfg(windows)]
fn output_route() -> Option<(&'static str, Option<String>)> {
    crate::backend::win_audio::output_route()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn output_route() -> Option<(&'static str, Option<String>)> {
    // Linux needs PulseAudio, which is its own change. `None` means "not
    // asked" and the handler emits nothing -- the same rule every probe
    // follows, rather than a zero standing in for a fact.
    None
}

/// `None` when the platform will not say. Not `Some(false)`: "not silenced"
/// and "we cannot tell" are different claims, and only one of them should
/// reach a dataset that exists to be honest about what it saw.
/// `SHQueryUserNotificationState` — the documented question, asked directly.
///
/// The first attempt read a registry value, and testing killed it: turning Do
/// Not Disturb on wrote nothing there. Windows 11 keeps the state in CloudStore
/// blobs whose shape is undocumented and changes between builds, which is
/// exactly the kind of thing this file should not be parsing.
///
/// This call is what the shell itself uses to decide whether to show a toast,
/// and it distinguishes the reasons. That matters: a quiet hour because the
/// person asked for one and a quiet hour because a game was fullscreen are
/// different facts about the day, and `mode` keeps them apart instead of
/// flattening both into "silenced".
/// Windows 11's Do Not Disturb, from the quiet-hours profile it actually sets.
///
/// Found by measurement, not by reading docs. Two earlier attempts were wrong:
/// a registry DWORD that does not exist on this machine at all, and
/// `SHQueryUserNotificationState`, which kept reporting ACCEPTS_NOTIFICATIONS
/// with the toggle on -- `QUNS_QUIET_TIME` predates Focus and does not track
/// it. Snapshotting the CloudStore keys with the toggle on and again with it
/// off left exactly one value different, and the bytes that moved were a
/// UTF-16 string:
///
/// ```text
/// on  -> Microsoft.QuietHoursProfile.PriorityOnly
/// off -> Microsoft.QuietHoursProfile.Unrestricted
/// ```
///
/// So the blob is searched for the profile id rather than parsed. The
/// surrounding format is undocumented and changes between builds; a substring
/// search survives that, where an offset would not. If Microsoft renames the
/// profiles this returns `None` and the signal goes quiet, which is the right
/// failure -- better than a confident wrong answer about whether somebody
/// asked to be left alone.
#[cfg(windows)]
fn quiet_hours_profile() -> Option<&'static str> {
    // A raw string, so the backslashes are path separators rather than escapes.
    const STORE: &str = concat!(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\CloudStore\Store\DefaultAccount\Current\",
        r"{214d800f-eb24-4181-9218-225be930b3ac}$windows.data.donotdisturb.quiethourssettings\",
        r"windows.data.donotdisturb.quiethourssettings",
    );
    let blob = crate::backend::reg_binary(STORE, "Data")?;
    // The string is not aligned to any fixed offset, so match on the encoded
    // bytes wherever they land.
    let find = |needle: &str| {
        let want: Vec<u8> = needle.encode_utf16().flat_map(u16::to_le_bytes).collect();
        blob.windows(want.len()).any(|w| w == want)
    };
    if find("PriorityOnly") {
        Some("priority_only")
    } else if find("AlarmsOnly") {
        Some("alarms_only")
    } else if find("Unrestricted") {
        Some("off")
    } else {
        None
    }
}

#[cfg(windows)]
fn dnd_state() -> Option<(bool, &'static str)> {
    use windows_sys::Win32::UI::Shell::SHQueryUserNotificationState;

    // The profile is what the PERSON chose, and it answers the question this
    // signal exists for. Checked first, because the API below cannot see it.
    match quiet_hours_profile() {
        Some("priority_only") => return Some((true, "priority_only")),
        Some("alarms_only") => return Some((true, "alarms_only")),
        // "off" falls through: the person is not suppressing anything, but the
        // system might still be, and that is worth knowing separately.
        _ => {}
    }

    let mut state = 0i32;
    let hr = unsafe { SHQueryUserNotificationState(&mut state) };
    if hr < 0 {
        return None;
    }
    // QUNS_*, from ShellAPI.h.
    match state {
        1 => None,                           // NOT_PRESENT: no shell yet
        2 => Some((true, "busy")),           // BUSY
        3 => Some((true, "fullscreen")),     // RUNNING_D3D_FULL_SCREEN
        4 => Some((true, "presentation")),   // PRESENTATION_MODE
        5 => Some((false, "on")),            // ACCEPTS_NOTIFICATIONS
        6 => Some((true, "quiet_time")),     // QUIET_TIME -- Do Not Disturb
        7 => Some((true, "app_fullscreen")), // APP: a fullscreen app
        _ => None,
    }
}

#[cfg(not(windows))]
fn dnd_state() -> Option<(bool, &'static str)> {
    do_not_disturb_unix().map(|on| (on, if on { "quiet_time" } else { "on" }))
}

#[cfg(target_os = "macos")]
fn do_not_disturb_unix() -> Option<bool> {
    // Focus modes write an assertion file while one is active. Reading it is
    // cheap and needs no permission; parsing it properly would mean a plist
    // reader, and the only question here is whether any assertion is live.
    let home = std::env::var("HOME").ok()?;
    let path = std::path::Path::new(&home).join("Library/DoNotDisturb/DB/Assertions.json");
    let body = std::fs::read_to_string(path).ok()?;
    Some(body.contains("\"storeAssertionRecords\"") && body.contains("assertionDetails"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn do_not_disturb_unix() -> Option<bool> {
    // Desktop-environment specific and not written.
    None
}
