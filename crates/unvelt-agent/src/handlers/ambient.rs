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
    last_dnd: Option<bool>,
}

impl AmbientHandler {
    pub fn new(cfg: &Config) -> Self {
        AmbientHandler {
            interval: cfg.context_sec as f64,
            last_route: None,
            last_dnd: None,
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

        if let Some(on) = do_not_disturb() {
            if self.last_dnd != Some(on) {
                self.last_dnd = Some(on);
                out.push(tick.event(
                    "desktop",
                    "dnd",
                    tick.now,
                    format!("dt:{}:dnd:{}:{}", tick.cfg.did, on as u8, tick.now),
                    Some(serde_json::json!({ "on": on as u8 })),
                ));
            }
        }

        out
    }
}

#[cfg(target_os = "macos")]
fn output_route() -> Option<(&'static str, Option<String>)> {
    crate::backend::mac_av::output_route()
}

#[cfg(not(target_os = "macos"))]
fn output_route() -> Option<(&'static str, Option<String>)> {
    // Windows needs IMMDeviceEnumerator through COM, which is a chunk of work
    // for one field; Linux needs PulseAudio. Neither is written. `None` means
    // "not asked", and the handler simply emits nothing -- which is the same
    // rule every probe follows rather than a zero standing in for a fact.
    None
}

/// `None` when the platform will not say. Not `Some(false)`: "not silenced"
/// and "we cannot tell" are different claims, and only one of them should
/// reach a dataset that exists to be honest about what it saw.
#[cfg(windows)]
fn do_not_disturb() -> Option<bool> {
    // Windows records the global toast switch here. Focus Assist has its own,
    // messier home under CloudStore that changes shape between builds; this
    // key is the one that has been stable and it answers the question that
    // matters -- were toasts allowed through at all.
    //
    // The value only exists once somebody has changed the setting, so on a
    // machine that never touched it this returns None and no `desktop.dnd`
    // event is ever emitted. That is deliberate. The default IS toasts-on, so
    // `Some(false)` would usually be right -- but "usually right" is an
    // inference about Windows rather than an observation of this machine, and
    // absence is never evidence here. The signal appears the first time the
    // setting is touched, and until then the digest correctly has nothing.
    let v = crate::backend::reg_dword(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Notifications\\Settings",
        "NOC_GLOBAL_SETTING_TOASTS_ENABLED",
    )?;
    Some(v == 0)
}

#[cfg(target_os = "macos")]
fn do_not_disturb() -> Option<bool> {
    // Focus modes write an assertion file while one is active. Reading it is
    // cheap and needs no permission; parsing it properly would mean a plist
    // reader, and the only question here is whether any assertion is live.
    let home = std::env::var("HOME").ok()?;
    let path = std::path::Path::new(&home).join("Library/DoNotDisturb/DB/Assertions.json");
    let body = std::fs::read_to_string(path).ok()?;
    Some(body.contains("\"storeAssertionRecords\"") && body.contains("assertionDetails"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn do_not_disturb() -> Option<bool> {
    // Desktop-environment specific and not written.
    None
}
