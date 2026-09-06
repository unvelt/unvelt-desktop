//! The per-app content allowlist.
//!
//! Empty until somebody adds an app by hand, and nothing is ever added for
//! them. That is the whole design: `notif.posted` normally carries the app and
//! the time and nothing else, and this is the narrow, per-app, opt-in
//! exception for the handful of apps where the CONTENT is what a person wants
//! to remember.
//!
//! WHY IT IS A LIST OF APPS AND NOT A SWITCH
//!
//! "Show me notification content" is not a question anybody can answer well in
//! one go: the same person who wants to remember what a calendar reminder said
//! does not want their bank balance or a private message stored. A single
//! switch forces one answer for all of them, so this asks per app, and the app
//! list is drawn from what has actually notified this machine rather than
//! typed from memory.
//!
//! NOT ENCRYPTED YET
//!
//! The registry marks `notif.posted.title` and `.text` as `depth: full,
//! is_encrypted: true`, and the encryption is not built -- these land in
//! `raw.events` as plaintext. That is a deliberate, informed decision to ship
//! without it for a single-user deployment, not an oversight, and the app says
//! so on the screen where the choice is made. It has to stop being true before
//! anyone outside the team is offered this.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// On disk. `seen` is not a second allowlist and grants nothing -- it is the
/// list the picker is drawn from, so the choice is made by recognising an app
/// rather than by typing an AUMID from memory.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Saved {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    seen: Vec<String>,
}

#[derive(Clone)]
pub struct Allowlist {
    apps: Arc<Mutex<BTreeSet<String>>>,
    seen: Arc<Mutex<BTreeSet<String>>>,
    path: PathBuf,
}

static SHARED: OnceLock<Allowlist> = OnceLock::new();

impl Allowlist {
    /// One instance per process, shared by handle.
    ///
    /// The window and the collector are the same process, and if each loaded
    /// its own copy the switch would write a file the running handler never
    /// re-reads -- turning content capture on would appear to work and do
    /// nothing until the next restart, and turning it OFF would appear to work
    /// and keep capturing. The second one is the reason this is a singleton.
    pub fn load() -> Allowlist {
        SHARED.get_or_init(Allowlist::read).clone()
    }

    fn read() -> Allowlist {
        let path = crate::config::state_dir().join("notif_content.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        // A bare array is the 0.3.0-dev format, before `seen` existed.
        let saved: Saved = serde_json::from_str(&raw).unwrap_or_else(|_| Saved {
            allow: serde_json::from_str(&raw).unwrap_or_default(),
            seen: Vec::new(),
        });
        Allowlist {
            apps: Arc::new(Mutex::new(saved.allow.into_iter().collect())),
            seen: Arc::new(Mutex::new(saved.seen.into_iter().collect())),
            path,
        }
    }

    /// An app has just interrupted this machine, so it can be offered in the
    /// picker. Records the app id ONLY -- never anything about the
    /// notification -- and is called whether or not the app is allowlisted.
    pub fn note_seen(&self, app: &str) {
        if app.is_empty() {
            return;
        }
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        // A ceiling, because this file is written from a polling loop and an
        // unbounded set is a slow leak. Apps that actually notify a person
        // number in the tens.
        if seen.contains(app) || seen.len() >= 200 {
            return;
        }
        seen.insert(app.to_string());
        drop(seen);
        self.save();
    }

    pub fn seen(&self) -> Vec<String> {
        self.seen
            .lock()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn save(&self) {
        let (Ok(apps), Ok(seen)) = (self.apps.lock(), self.seen.lock()) else {
            return;
        };
        let saved = Saved {
            allow: apps.iter().cloned().collect(),
            seen: seen.iter().cloned().collect(),
        };
        if let Ok(json) = serde_json::to_string(&saved) {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&self.path, json);
        }
    }

    pub fn allows(&self, app: &str) -> bool {
        // A poisoned lock must fail CLOSED here, unlike the source toggles.
        // Getting this backwards would capture content nobody agreed to, and
        // that is not a mistake a later release can take back.
        self.apps.lock().map(|a| a.contains(app)).unwrap_or(false)
    }

    pub fn list(&self) -> Vec<String> {
        self.apps
            .lock()
            .map(|a| a.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn set(&self, app: &str, on: bool) {
        if let Ok(mut a) = self.apps.lock() {
            if on {
                a.insert(app.to_string());
            } else {
                a.remove(app);
            }
        }
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> Allowlist {
        Allowlist {
            apps: Arc::new(Mutex::new(BTreeSet::new())),
            seen: Arc::new(Mutex::new(BTreeSet::new())),
            path: std::env::temp_dir().join("unvelt-allow-test.json"),
        }
    }

    #[test]
    fn being_seen_is_not_being_allowed() {
        // The picker's list and the capture list are different facts. If
        // `note_seen` ever granted anything, every app that ever notified you
        // would have its content kept.
        let a = empty();
        a.note_seen("WhatsApp");
        assert_eq!(a.seen(), vec!["WhatsApp".to_string()]);
        assert!(!a.allows("WhatsApp"));
        assert!(a.list().is_empty());
    }

    #[test]
    fn the_seen_list_is_bounded_and_ignores_blanks() {
        let a = empty();
        a.note_seen("");
        assert!(a.seen().is_empty());
        for i in 0..250 {
            a.note_seen(&format!("app{i}"));
        }
        assert_eq!(a.seen().len(), 200);
    }

    #[test]
    fn nothing_is_allowed_until_it_is_named() {
        let a = empty();
        assert!(!a.allows("WhatsApp"));
        assert!(!a.allows(""));
        assert!(a.list().is_empty());
    }

    #[test]
    fn an_app_is_allowed_only_by_its_exact_key() {
        // Substring or prefix matching here would be a quiet disaster: adding
        // "Mail" must not enrol "MailChimp", and adding one WSA app must not
        // enrol every app inside Windows Subsystem for Android, which all
        // share a long prefix.
        let a = empty();
        a.set(
            "MicrosoftCorporationII.WindowsSubsystemForAndroid_8wekyb3d8bbwe!in.startv.hotstar",
            true,
        );
        assert!(a.allows(
            "MicrosoftCorporationII.WindowsSubsystemForAndroid_8wekyb3d8bbwe!in.startv.hotstar"
        ));
        assert!(!a.allows("MicrosoftCorporationII.WindowsSubsystemForAndroid_8wekyb3d8bbwe!com.google.android.gms"));
        assert!(!a.allows("MicrosoftCorporationII.WindowsSubsystemForAndroid_8wekyb3d8bbwe"));
    }
}
