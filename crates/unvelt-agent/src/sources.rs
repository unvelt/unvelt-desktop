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
pub const CATALOGUE: &[(&str, &str, &str, &str)] = &[
    (
        "focus",
        "Apps and window titles",
        "Which app you were in and for how long. The backbone of everything \
         else: without it a day has no shape.",
        "Window titles, which often carry a document name, a chat, or the \
         subject of what you were reading.",
    ),
    (
        "activity",
        "Typing and mouse intensity",
        "Whether you were working or watching. A ratio per minute separates \
         two hours of writing from two hours of video.",
        "How often input happened. Never a keystroke, never what was typed.",
    ),
    (
        "session",
        "Lock and unlock",
        "When you stepped away and came back, which is what makes a gap \
         readable as absence rather than a dead collector.",
        "The times you locked and unlocked this machine.",
    ),
    (
        "location",
        "Wi-Fi and monitors",
        "Home against office, without GPS. The network and the number of \
         screens are the cheapest place proxy there is.",
        "The network name and its gateway address. Both identify a place.",
    ),
    (
        "power",
        "Battery and charging",
        "Docked against carried, and the times the machine was on the move.",
        "Battery level and whether it is on mains.",
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
        "Notifications",
        "Which apps interrupt you, how often, and whether you acted on them.",
        "The app and the time. The content of a notification only for apps you \
         pick one by one, and that list starts empty.",
    ),
    (
        "Music and video",
        "What you listened to and for how long, and what was playing while you \
         worked.",
        "The app, the track and the artist where the app publishes them.",
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
    path: PathBuf,
}

impl Toggles {
    pub fn load() -> Toggles {
        let path = crate::config::state_dir().join("sources.json");
        let disabled = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .map(|v| v.into_iter().collect())
            .unwrap_or_default();
        Toggles {
            disabled: Arc::new(Mutex::new(disabled)),
            path,
        }
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        // A poisoned lock must not silently switch collection off, so the
        // failure direction here is "keep doing what was asked".
        self.disabled
            .lock()
            .map(|d| !d.contains(name))
            .unwrap_or(true)
    }

    pub fn set(&self, name: &str, on: bool) {
        if let Ok(mut d) = self.disabled.lock() {
            if on {
                d.remove(name);
            } else {
                d.insert(name.to_string());
            }
            let list: Vec<&String> = d.iter().collect();
            if let Ok(json) = serde_json::to_string(&list) {
                if let Some(dir) = self.path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&self.path, json);
            }
        }
    }

    pub fn list(&self) -> Vec<Source> {
        CATALOGUE
            .iter()
            .map(|(id, label, gives, costs)| Source {
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
            .flat_map(|(_, l, g, c)| [*l, *g, *c])
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
