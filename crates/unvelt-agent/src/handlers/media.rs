//! What was playing, from the System Media Transport Controls.
//!
//! SMTC is the strip that appears when you press a media key: every app that
//! wants those keys to work registers a session with it. Reading it needs no
//! package identity and no signature — verified on real hardware, from an
//! unpackaged process, before this was planned around.
//!
//! ONE CONSTRAINT THAT DECIDES THE DEPLOYMENT
//!
//! SMTC does not exist in a non-interactive session. A Windows *service* gets
//! nothing from it, ever. That is why the collector runs as the logged-in user
//! rather than as a service, and why nothing here should be "fixed" by moving
//! it to one.
//!
//! WHAT IT EMITS, AND WHY THE SAME EVENTS AS THE PHONE
//!
//! `media.play`, `resume` and `pause`, exactly as the Android collector sends
//! them. A desktop is a device, not a vocabulary: the `media_play` interval
//! and `media_min` metric already exist and work on the first desktop event,
//! and inventing `desktop.media` would have meant a second copy of every one
//! of them. See the note at the top of `db/registry/work.yaml`.
//!
//! Browsers appear here the same way Spotify does — SMTC reports Brave playing
//! a track with no marker saying it came from a tab. Whether a browser should
//! count as listening time is still open, and it is open on Android too; it is
//! a question for the digest, not for the collector, which reports what the
//! platform said.

use super::{Handler, Tick};
use crate::config::Config;
use crate::envelope::Event;

/// What the platform currently reports. `None` when nothing is playing.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Now {
    pub app: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
}

pub struct MediaHandler {
    interval: f64,
    last: Option<Now>,
    warned: bool,
}

impl MediaHandler {
    pub fn new(cfg: &Config) -> Self {
        MediaHandler {
            // Fast enough to catch a track change, slow enough that a WinRT
            // round trip per tick is not the agent's main cost.
            interval: (cfg.sample_sec as f64).max(10.0),
            last: None,
            warned: false,
        }
    }
}

impl Handler for MediaHandler {
    fn name(&self) -> &'static str {
        "media"
    }

    fn interval(&self) -> f64 {
        self.interval
    }

    fn poll(&mut self, tick: &mut Tick) -> Vec<Event> {
        let now = match current() {
            Ok(n) => n,
            Err(e) => {
                if tick.cfg.debug && !self.warned {
                    eprintln!("unvelt: media session unavailable: {e}");
                    self.warned = true;
                }
                return Vec::new();
            }
        };

        let prev = self.last.take();
        self.last = now.clone();
        let did = &tick.cfg.did;
        let ts = tick.now;

        // Three transitions, and they are not interchangeable. `play` means a
        // new track started; `resume` means the same one was un-paused. The
        // digest counts plays and measures listening time separately, so
        // collapsing them would make a paused-and-resumed album look like a
        // dozen tracks.
        match (prev, now) {
            // The session vanished entirely -- the app quit while playing.
            // Reported with the app and track it WAS playing, which is the
            // only useful answer and the reason `prev` is matched rather than
            // the state we just stored.
            (Some(p), None) if p.playing => vec![tick.event(
                "media",
                "pause",
                ts,
                format!("md:{did}:pause:{ts}"),
                Some(serde_json::json!({"app": p.app, "title": p.title})),
            )],
            (_, None) => Vec::new(),
            (None, Some(n)) if n.playing => vec![play_event(tick, &n)],
            (Some(p), Some(n)) => {
                if n.playing && (p.title != n.title || p.app != n.app) {
                    vec![play_event(tick, &n)]
                } else if n.playing && !p.playing {
                    vec![tick.event(
                        "media",
                        "resume",
                        ts,
                        format!("md:{did}:resume:{ts}"),
                        Some(serde_json::json!({"app": n.app, "title": n.title})),
                    )]
                } else if !n.playing && p.playing {
                    vec![tick.event(
                        "media",
                        "pause",
                        ts,
                        format!("md:{did}:pause:{ts}"),
                        Some(serde_json::json!({"app": n.app, "title": n.title})),
                    )]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }
}

fn play_event(tick: &Tick, n: &Now) -> Event {
    let mut p = serde_json::Map::new();
    p.insert("app".into(), serde_json::json!(n.app));
    // Absent rather than empty. Many apps never populate these, and an empty
    // string would be indistinguishable from a track genuinely called "".
    for (k, v) in [
        ("title", &n.title),
        ("artist", &n.artist),
        ("album", &n.album),
    ] {
        if !v.is_empty() {
            p.insert(k.into(), serde_json::json!(v));
        }
    }
    tick.event(
        "media",
        "play",
        tick.now,
        format!("md:{}:play:{}", tick.cfg.did, tick.now),
        Some(serde_json::Value::Object(p)),
    )
}

/// The current session, for `--probe`.
///
/// Exposed so the media path can be checked on a machine without waiting for
/// a track change. SMTC is the one probe that cannot be verified by staring
/// at a silent computer, and "it emitted nothing" and "it is broken" look
/// identical from outside.
pub fn probe() -> Result<Option<Now>, String> {
    current()
}

#[cfg(windows)]
fn current() -> Result<Option<Now>, String> {
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSessionManager as Manager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    };

    let mgr = Manager::RequestAsync()
        .map_err(|e| e.to_string())?
        .get()
        .map_err(|e| e.to_string())?;
    let session = match mgr.GetCurrentSession() {
        Ok(s) => s,
        // No current session is the normal state of a machine with nothing
        // playing, not an error worth reporting.
        Err(_) => return Ok(None),
    };
    let info = session.GetPlaybackInfo().map_err(|e| e.to_string())?;
    let status = info.PlaybackStatus().map_err(|e| e.to_string())?;
    let props = session
        .TryGetMediaPropertiesAsync()
        .map_err(|e| e.to_string())?
        .get()
        .map_err(|e| e.to_string())?;

    let s = |r: Result<windows::core::HSTRING, windows::core::Error>| {
        r.map(|h| h.to_string_lossy()).unwrap_or_default()
    };
    Ok(Some(Now {
        // The AUMID, which is the same namespace `notif.posted` reports, so
        // one app has one key across both sources on this platform.
        app: s(session.SourceAppUserModelId()),
        title: s(props.Title()),
        artist: s(props.Artist()),
        album: s(props.AlbumTitle()),
        playing: status == Status::Playing,
    }))
}

#[cfg(not(windows))]
fn current() -> Result<Option<Now>, String> {
    // MPRIS2 on Linux and the three-layer macOS design belong here; both are
    // their own change, and returning "nothing playing" is the honest interim
    // answer rather than pretending the platform said so.
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_play_event_omits_fields_the_app_never_set() {
        // An empty title is not a track called "". Many apps populate none of
        // these, and the registry's own note says so: TITLE is a convention,
        // not a contract.
        let cfg = Config::for_test();
        let mut b = crate::backend::make_backend(&cfg);
        let tick = Tick {
            cfg: &cfg,
            now: 1,
            idle: 0.0,
            away: false,
            backend: b.as_mut(),
        };
        let e = play_event(
            &tick,
            &Now {
                app: "Spotify".into(),
                title: "PANGA".into(),
                artist: String::new(),
                album: String::new(),
                playing: true,
            },
        );
        let p = e.p.unwrap();
        assert_eq!(p["app"], "Spotify");
        assert_eq!(p["title"], "PANGA");
        assert!(p.get("artist").is_none(), "empty artist was sent");
        assert!(p.get("album").is_none(), "empty album was sent");
    }
}
