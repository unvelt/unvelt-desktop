//! Which apps are making sound — `desktop.playing`, and deliberately not
//! `media.play`.
//!
//! WHY THIS IS NOT MEDIA
//!
//! macOS has no system-wide now-playing for browsers. AppleScript reaches Music
//! and Spotify because they publish a scripting dictionary; Chrome and Brave
//! publish nothing about which tab is audible. So Core Audio's audible-process
//! list is the only way to see music played in a tab at all.
//!
//! But audible is broader than media. A notification ding, a video call and a
//! game all make sound. Folding that into `media_min` would turn a two-hour
//! Zoom into two hours of listening — and the phone would then disagree with
//! the laptop about the same day, which is exactly the kind of cross-device
//! total the row-shape rules exist to prevent.
//!
//! So it gets its own event, its own interval (`app_audio`) and its own metric
//! (`app_audio_min`), and nothing ever sums the two. `media_min` is listening;
//! this is audio. Where both see the same Spotify play, they overlap on
//! purpose and a sum of them would be minutes that did not happen.
//!
//! The duration floor lives in the derivation rather than here: the collector
//! reports what the platform said, and deciding that a three-second ding is not
//! listening is a judgement for the digest.

use std::collections::BTreeSet;

use super::{Handler, Tick};
use crate::config::Config;
use crate::envelope::Event;

pub struct AudibleHandler {
    interval: f64,
    last: Option<BTreeSet<String>>,
}

impl AudibleHandler {
    pub fn new(cfg: &Config) -> Self {
        AudibleHandler {
            interval: (cfg.sample_sec as f64).max(10.0),
            last: None,
        }
    }
}

impl Handler for AudibleHandler {
    fn name(&self) -> &'static str {
        // Shares the media switch. Someone who said "you may see what I listen
        // to" has answered this question too, and a second switch for the same
        // subject would be a distinction only the implementation cares about.
        "media"
    }

    fn interval(&self) -> f64 {
        self.interval
    }

    fn poll(&mut self, tick: &mut Tick) -> Vec<Event> {
        let now = audible();
        let prev = match self.last.replace(now.clone()) {
            // Seed without emitting: an app already making sound when the
            // agent starts did not start then.
            None => return Vec::new(),
            Some(p) => p,
        };
        let mut out = Vec::new();
        for app in now.difference(&prev) {
            out.push(edge(tick, app, 1));
        }
        for app in prev.difference(&now) {
            out.push(edge(tick, app, 0));
        }
        out
    }
}

fn edge(tick: &Tick, app: &str, on: u8) -> Event {
    tick.event(
        "desktop",
        "playing",
        tick.now,
        // The app and direction are in the key: several apps can start or stop
        // in one tick and a timestamp alone would collide them.
        format!("dt:{}:play:{app}:{on}:{}", tick.cfg.did, tick.now),
        Some(serde_json::json!({ "app": app, "on": on })),
    )
}

#[cfg(target_os = "macos")]
fn audible() -> BTreeSet<String> {
    crate::backend::mac_av::audible_apps()
}

#[cfg(not(target_os = "macos"))]
fn audible() -> BTreeSet<String> {
    // Windows needs none of this: SMTC reports browsers directly, with the
    // track, so `media.play` already covers what this exists to rescue. Linux
    // would need PulseAudio sink-input polling, which is its own change.
    BTreeSet::new()
}
