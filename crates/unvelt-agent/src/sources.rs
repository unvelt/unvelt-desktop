//! What this machine collects, and the switches for it.
//!
//! This is the **local per-device toggle**, and it is not the consent ledger.
//! Consent is a per-user decision recorded server-side in
//! `core.consent_grants`: a person's stance on "may we see your notifications"
//! is about the data, not about which machine produced it. This is the other
//! question — "not from this laptop" — and it lives here because it is about
//! this laptop and nothing else needs to know.
//!
//! Turning one off does not delete anything already collected and does not
//! revoke consent. It stops this machine reading that signal, and the silence
//! that follows is a `meta.gap`, not an absence of behaviour — the distinction
//! coverage exists to preserve.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// What is written to disk: only answers a person actually gave.
///
/// Two lists rather than one, because "off" and "not asked" are different
/// facts and only one of them should survive a change of default. Someone who
/// turned notifications on must not find them off again because a later
/// release changed what a fresh install does.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Saved {
    #[serde(default)]
    off: Vec<String>,
    #[serde(default)]
    on: Vec<String>,
}

/// One switch, and enough words to decide with.
///
/// `gives` and `costs` are both required, and the cost is not optional
/// politeness: a consent screen that lists only benefits is a sales page. Each
/// entry says what turning it on lets us answer AND what it lets us see.
#[derive(serde::Serialize, Clone, Debug)]
pub struct Source {
    pub id: &'static str,
    pub label: &'static str,
    pub gives: &'static str,
    pub costs: &'static str,
    pub enabled: bool,
}

/// Keyed by handler name, so the controller can gate on it directly.
///
/// The last field is whether it is on when nobody has said otherwise, and the
/// two that are off matter more than the five that are on.
///
/// On Android, notifications and media sit behind an OS permission the person
/// grants by hand. On Windows they sit behind nothing: the notification store
/// is a readable file and SMTC answers any process that asks. So an update
/// shipping them on by default would start collecting who interrupts you and
/// what you listen to without anyone agreeing to it — and without the OS
/// asking on our behalf either. When the platform declines to put the
/// question, this switch is the only place it gets put. So it gets put, and
/// the answer starts at no.
pub const CATALOGUE: &[(&str, &str, &str, &str, bool)] = &[
    (
        "focus",
        "Apps and window titles",
        "Which app you were in and for how long. The backbone of everything \
         else: without it a day has no shape.",
        "Window titles, which often carry a document name, a chat, or the \
         subject of what you were reading.",
        true,
    ),
    (
        "activity",
        "Typing and mouse intensity",
        "Whether you were working or watching. A ratio per minute separates \
         two hours of writing from two hours of video.",
        "How often input happened. Never a keystroke, never what was typed.",
        true,
    ),
    (
        "session",
        "Lock and unlock",
        "When you stepped away and came back, which is what makes a gap \
         readable as absence rather than a dead collector.",
        "The times you locked and unlocked this machine.",
        true,
    ),
    (
        "location",
        "Wi-Fi and monitors",
        "Home against office, without GPS. The network and the number of \
         screens are the cheapest place proxy there is.",
        "The network name and its gateway address. Both identify a place.",
        true,
    ),
    (
        "power",
        "Battery and charging",
        "Docked against carried, and the times the machine was on the move.",
        "Battery level and whether it is on mains.",
        true,
    ),
    (
        "notif",
        "Notifications",
        "Which apps interrupt you and how often. Interruptions are most of \
         what makes a day feel fragmented, and nothing else can see them.",
        "The app that sent each notification, and when. Never the title and \
         never the message — the query that reads them cannot return the \
         content at all, which is checked by a test rather than promised in a \
         document. On a Mac this needs Full Disk Access, which macOS will ask \
         you for; on Windows it needs no permission at all, which is why this \
         switch exists.",
        false,
    ),
    (
        "media",
        "Music and video",
        "What you listened to and for how long, and what was playing while \
         you worked.",
        "The app, and the track, artist and album where the app publishes \
         them to the system media controls.",
        false,
    ),
];

/// Signals unvelt will collect but cannot yet.
///
/// Shown on the consent screen with no switch. Two reasons, and the second is
/// the important one: a switch that does nothing is a lie, and a consent
/// screen that lists only today's five signals lets someone agree to unvelt
/// without knowing what unvelt is going to become. Saying "not yet" is how
/// the screen stays true in both directions.
pub const PLANNED: &[(&str, &str, &str)] = &[
    (
        "Notification content",
        "What an interruption was actually about, for the handful of apps \
         where that matters to you.",
        "The title and body of notifications, and only from apps you add one \
         at a time. The list starts empty and nothing is added to it for you.",
    ),
    (
        "Camera and microphone",
        "When you were in a call. Two hours on a call looks like two hours away \
         from the keyboard, and this is the only thing that can say otherwise.",
        "Whether the camera or microphone was live, and which app had it. Never \
         what was said, never what was seen.",
    ),
];

#[derive(Clone)]
pub struct Toggles {
    disabled: Arc<Mutex<BTreeSet<String>>>,
    enabled: Arc<Mutex<BTreeSet<String>>>,
    path: PathBuf,
}

impl Toggles {
    pub fn load() -> Toggles {
        let path = crate::config::state_dir().join("sources.json");
        let saved: Saved = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Toggles {
            disabled: Arc::new(Mutex::new(saved.off.into_iter().collect())),
            enabled: Arc::new(Mutex::new(saved.on.into_iter().collect())),
            path,
        }
    }

    fn save(&self) {
        let (Ok(off), Ok(on)) = (self.disabled.lock(), self.enabled.lock()) else {
            return;
        };
        let saved = Saved {
            off: off.iter().cloned().collect(),
            on: on.iter().cloned().collect(),
        };
        if let Ok(json) = serde_json::to_string(&saved) {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&self.path, json);
        }
    }

    fn default_on(name: &str) -> bool {
        CATALOGUE
            .iter()
            .find(|(id, ..)| *id == name)
            .map(|(.., on)| *on)
            .unwrap_or(true)
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        // Three states, not two: off because the person said so, on because
        // they said so, or never asked. A poisoned lock must not silently
        // change what is collected in either direction, so both failures fall
        // through to the declared default.
        let off = self
            .disabled
            .lock()
            .map(|d| d.contains(name))
            .unwrap_or(false);
        if off {
            return false;
        }
        let on = self
            .enabled
            .lock()
            .map(|d| d.contains(name))
            .unwrap_or(false);
        on || Self::default_on(name)
    }

    pub fn set(&self, name: &str, on: bool) {
        if let (Ok(mut off), Ok(mut yes)) = (self.disabled.lock(), self.enabled.lock()) {
            if on {
                off.remove(name);
                yes.insert(name.to_string());
            } else {
                yes.remove(name);
                off.insert(name.to_string());
            }
        }
        self.save();
    }

    pub fn list(&self) -> Vec<Source> {
        CATALOGUE
            .iter()
            .map(|(id, label, gives, costs, _)| Source {
                id,
                label,
                gives,
                costs,
                enabled: self.is_enabled(id),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a mangling that has already happened once: a Rust line
    /// continuation inside a shell heredoc collapsed into a run of spaces, and
    /// the sentence shipped with a gap in the middle of it.
    #[test]
    fn no_copy_has_a_run_of_spaces_in_it() {
        let all = CATALOGUE
            .iter()
            .flat_map(|(_, l, g, c, _)| [*l, *g, *c])
            .chain(PLANNED.iter().flat_map(|(l, g, c)| [*l, *g, *c]));
        for text in all {
            assert!(!text.contains("  "), "double space in copy: {text}");
        }
    }

    #[test]
    fn planned_signals_say_what_they_will_see() {
        // Same rule as the live ones. A "coming soon" entry that lists only
        // the benefit is how a consent screen becomes marketing.
        for (label, gives, costs) in PLANNED {
            assert!(!label.trim().is_empty());
            assert!(!gives.trim().is_empty(), "{label} has no `gives`");
            assert!(!costs.trim().is_empty(), "{label} has no `costs`");
        }
    }

    #[test]
    fn every_source_says_what_it_costs() {
        // The cost line is load bearing, not decoration: a screen that lists
        // only what a signal gives you is a sales page, and this is the screen
        // someone decides on.
        for s in Toggles::load().list() {
            assert!(!s.gives.trim().is_empty(), "{} has no `gives`", s.id);
            assert!(!s.costs.trim().is_empty(), "{} has no `costs`", s.id);
        }
    }

    #[test]
    fn the_two_new_sources_are_off_until_someone_says_otherwise() {
        // The consent decision this release turns on. Windows asks nothing
        // before letting a process read the notification store or the media
        // session, so if these ever default to true the only gate is gone.
        assert!(
            !Toggles::default_on("notif"),
            "notifications must default off"
        );
        assert!(!Toggles::default_on("media"), "media must default off");
        for on_by_default in ["focus", "activity", "session", "location", "power"] {
            assert!(
                Toggles::default_on(on_by_default),
                "{on_by_default} regressed"
            );
        }
    }

    #[test]
    fn an_explicit_yes_outlives_a_change_of_default() {
        // Saved as an answer, not as an absence: turning notifications on and
        // then shipping a release that still defaults them off must not
        // quietly switch them back.
        let t = Toggles {
            disabled: Arc::new(Mutex::new(BTreeSet::new())),
            enabled: Arc::new(Mutex::new(["notif".to_string()].into_iter().collect())),
            path: std::env::temp_dir().join("unvelt-toggle-test.json"),
        };
        assert!(t.is_enabled("notif"));
        assert!(!t.is_enabled("media"));
    }

    #[test]
    fn the_catalogue_covers_exactly_the_default_handlers() {
        // A handler with no switch is a signal nobody can turn off, and a
        // switch with no handler is one that does nothing. Both are the kind
        // of drift that only shows up when someone trusts the screen.
        let cfg = crate::config::Config::for_test();
        let names: BTreeSet<&str> = crate::handlers::build_default(&cfg)
            .iter()
            .map(|h| h.name())
            .collect();
        let listed: BTreeSet<&str> = CATALOGUE.iter().map(|(id, ..)| *id).collect();
        assert_eq!(names, listed);
    }
}
