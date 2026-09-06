//! Foreground app and window title — the workhorse signal.
//!
//! Emits on change, and re-emits an unchanged foreground every `resample_sec`
//! so a long session stays visible to server-side sessionization rather than
//! looking like one event followed by silence. Silent while the user is away,
//! because `desktop.focus` is a poll and it will truthfully report whatever
//! held focus on an unattended machine all night.
//!
//! Also emits `inventory.app` the first time it sees an app whose display name
//! it knows. That pairing is not optional: migration 0015 keys `app_labels` on
//! (platform, pkg), so the release that starts sending a bundle id as the key
//! must also be the release that supplies the label for it — otherwise every
//! macOS breakdown reads `com.google.Chrome` until something else fills the
//! gap. Windows sends no label because it has none cheaply, and a missing
//! label renders as the key rather than as a wrong name.

use std::collections::HashSet;

use super::{Handler, Tick};
use crate::config::Config;
use crate::envelope::Event;

/// Coarse, and meant to be extended. Substring match on the lowercased app
/// name, then the title.
const CATS: &[(&str, &[&str])] = &[
    (
        "code",
        &[
            "code",
            "pycharm",
            "intellij",
            "webstorm",
            "goland",
            "xcode",
            "sublime",
            "alacritty",
            "iterm",
            "terminal",
            "wezterm",
            "kitty",
            "windowsterminal",
            "cmd",
            "powershell",
            "nvim",
            "vim",
        ],
    ),
    (
        "browser",
        &[
            "chrome", "brave", "firefox", "safari", "edge", "arc", "opera",
        ],
    ),
    (
        "comms",
        &[
            "slack", "discord", "zoom", "teams", "telegram", "whatsapp", "outlook", "mail",
        ],
    ),
    (
        "media",
        &["spotify", "vlc", "music", "youtube", "netflix", "quicktime"],
    ),
    (
        "game",
        &[
            "steam",
            "epicgames",
            "riotclient",
            "leagueclient",
            "valorant",
            "csgo",
            "cs2",
        ],
    ),
];

pub fn category(app: &str, title: &str, fullscreen: bool) -> &'static str {
    let hay = format!("{app} {title}").to_ascii_lowercase();
    for (cat, hints) in CATS {
        if hints.iter().any(|h| hay.contains(h)) {
            return cat;
        }
    }
    if fullscreen {
        // A fullscreen unknown is most often a game or a video.
        return "game";
    }
    "other"
}

pub struct FocusHandler {
    interval: f64,
    resample_ms: i64,
    last: Option<(String, String)>,
    last_ts: i64,
    labelled: HashSet<String>,
}

impl FocusHandler {
    pub fn new(cfg: &Config) -> Self {
        FocusHandler {
            interval: cfg.sample_sec as f64,
            resample_ms: cfg.resample_sec as i64 * 1000,
            last: None,
            last_ts: 0,
            labelled: HashSet::new(),
        }
    }
}

impl Handler for FocusHandler {
    fn name(&self) -> &'static str {
        "focus"
    }

    fn interval(&self) -> f64 {
        self.interval
    }

    fn poll(&mut self, tick: &mut Tick) -> Vec<Event> {
        if tick.away {
            // Force a fresh event when they come back, rather than treating
            // the pre-idle app as still in focus.
            self.last = None;
            return Vec::new();
        }
        let Some(front) = tick.backend.frontmost() else {
            return Vec::new();
        };
        let fs = tick.backend.fullscreen().unwrap_or(false);
        let key = (front.pkg.clone(), front.title.clone());
        if self.last.as_ref() == Some(&key) && (tick.now - self.last_ts) < self.resample_ms {
            return Vec::new();
        }
        self.last = Some(key);
        self.last_ts = tick.now;

        let mut out = Vec::new();

        // The label, once per app per run. Sent under `inventory` because that
        // is the source whose consent covers "which apps exist and what are
        // they called" -- inventing a second vocabulary for the same fact is
        // exactly what work.yaml forbids.
        if let Some(label) = front.label.as_deref() {
            if !label.is_empty() && label != front.pkg && self.labelled.insert(front.pkg.clone()) {
                out.push(tick.event(
                    "inventory",
                    "app",
                    tick.now,
                    format!("inv:{}:{}", tick.cfg.did, front.pkg),
                    Some(serde_json::json!({"pkg": front.pkg, "label": label})),
                ));
            }
        }

        let mut p = serde_json::json!({
            "app": front.pkg,
            "title": front.title,
            "cat": category(&front.pkg, &front.title, fs),
        });
        if fs {
            p["fs"] = serde_json::json!(1);
        }
        out.push(tick.event(
            "desktop",
            "focus",
            tick.now,
            format!("dt:{}:focus:{}", tick.cfg.did, tick.now),
            Some(p),
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_match_the_python_collector() {
        assert_eq!(
            category("chrome", "unvelt - Google Chrome", false),
            "browser"
        );
        assert_eq!(category("Code", "focus.rs", false), "code");
        assert_eq!(category("slack", "general", false), "comms");
        assert_eq!(category("spotify", "PANGA", false), "media");
        // Title alone is enough; the app name need not match.
        assert_eq!(category("explorer", "youtube - watch", false), "media");
        assert_eq!(category("notepad", "notes", false), "other");
        // An unknown fullscreen window is a game or a video, not "other".
        assert_eq!(category("someunknownapp", "", true), "game");
    }

    #[test]
    fn code_wins_over_browser_when_both_match() {
        // Ordering is the tie-break, and it is deliberate: an editor with a
        // docs tab in the title is still time spent in the editor.
        assert_eq!(category("code", "chrome devtools", false), "code");
    }
}
