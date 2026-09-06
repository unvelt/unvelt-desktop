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

/// macOS layer one: the apps that publish a scripting dictionary.
///
/// Music and Spotify, and nothing else. Browsers are the gap this cannot
/// close -- Chrome and Brave expose the active tab's title and URL and nothing
/// about which tab is audible -- and that gap is why `desktop.playing` exists.
/// A third layer (`mediaremote-adapter`) is the only route to browser TRACK
/// metadata on macOS and is not built; see docs/desktop-plan.md.
/// Spotify and Music, through their scripting dictionaries.
///
/// WHY NOT THE SYSTEM NOW-PLAYING, THE WAY WINDOWS DOES
///
/// SMTC hands any Windows process the current track for any app, browsers
/// included. macOS has no public equivalent, and the private one --
/// MediaRemote.framework -- is gated. Measured on macOS 15.6 (24G84, arm64)
/// with Chrome playing, by `tools/probe_mac_nowplaying.sh`:
///
/// ```text
/// unsigned      MRMediaRemoteGetNowPlayingInfo            -> (null)
///               MRMediaRemoteGetNowPlayingApplicationIsPlaying -> no
///               MRMediaRemoteGetNowPlayingClient          -> no client
/// ad-hoc signed  identical
/// entitled       SIGKILL at exec, every stage
/// ```
///
/// All five symbols resolve, so the framework has not moved: the calls
/// succeed and answer nothing. Since 15.4 `mediaremoted` gates clients by the
/// CALLING PROCESS'S code-signing identifier and answers only the `com.apple.*`
/// namespace, so a binary signed with our own identity -- or none -- gets an
/// empty dictionary, which is exactly what the table shows.
///
/// WHAT THE PROBE DID NOT TEST, AND WHY THE EARLIER "FULL STOP" WAS WRONG.
/// The gate is on the identity of the process that calls, not on the caller's
/// entitlements -- so it is defeated not by signing our binary better but by
/// making the call from a process Apple already trusts. `/usr/bin/perl` is
/// signed `com.apple.perl` AND carries `flags=0x0` (no hardened runtime, so no
/// library validation), which means it will `dlopen` an arbitrary unsigned
/// dylib of ours and then talk to `mediaremoted` with a trusted identity. That
/// is the `mediaremote-adapter` technique, and it does return the browser
/// track. My probe only ever tested a process gated by its OWN signature, and
/// I generalised "a normal app gets nothing" into "nothing gets it", which was
/// wrong.
///
/// So the way it is reached matters. unvelt ships NONE of the loophole. It
/// shells out to `media-control` (Homebrew: `ungive/media-control`), which
/// carries the mediaremote-adapter dylib and the perl loader, if that tool is
/// installed -- the cask declares it a dependency, so a brew install has it.
/// The code-signing trick is then ungive's to maintain and Apple's to close,
/// and the day it closes this degrades to the AppleScript path below rather
/// than breaking: a Mac with the tool sees browser tracks, a Mac without it
/// sees Spotify and Music, and neither carries an exploit we wrote.
///
/// It stays under the one `media` switch rather than a new one, because the
/// question a person answered -- "you may see what I listen to" -- is the same
/// question whether the track is in Spotify or a browser tab. The one real
/// consequence is that on a Mac with `media-control`, that switch now also
/// captures YouTube and the like, which it could not before; the cost line on
/// the switch already says "where the app publishes to the system media
/// controls", which is exactly what a browser does here.
#[cfg(target_os = "macos")]
fn current() -> Result<Option<Now>, String> {
    // media-control first: it reports every app registered with the system
    // now-playing, browsers included. Only when it is absent or unreadable do
    // we fall back to AppleScript, which reaches Spotify and Music alone.
    match media_control() {
        Some(res) => res,
        None => applescript_now(),
    }
}

/// `Some` when `media-control` was present and answered -- authoritatively,
/// including `Some(Ok(None))` for "nothing is playing". `None` only when the
/// tool is not installed or gave output we could not parse, which is the sole
/// case that should fall through to AppleScript.
#[cfg(target_os = "macos")]
fn media_control() -> Option<Result<Option<Now>, String>> {
    let bin = media_control_bin()?;
    let out = crate::backend::run(&bin, &["get"]);
    // The tool IS present, so its answer is authoritative either way: a track,
    // or a real "nothing playing". Only unparseable output falls through, and
    // parse_media_control draws that line.
    Some(Ok(parse_media_control(out.trim())?))
}

/// The pure half, so the JSON contract is testable without a Mac.
///
/// `None` means "unusable output, fall back to AppleScript"; `Some(None)`
/// means the tool answered and nothing is playing; `Some(Some)` is a track.
#[cfg(any(target_os = "macos", test))]
fn parse_media_control(out: &str) -> Option<Option<Now>> {
    if out.is_empty() {
        return Some(None);
    }
    let v: serde_json::Value = serde_json::from_str(out).ok()?;
    // `get` prints the payload at the top level; `stream` wraps it under
    // `payload`. Tolerate both so a version bump that changes which one `get`
    // emits does not silently stop collecting.
    let p = v.get("payload").unwrap_or(&v);
    let s = |k: &str| {
        p.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let (title, app) = (s("title"), s("bundleIdentifier"));
    if title.is_empty() && app.is_empty() {
        return Some(None);
    }
    // Play vs pause, from whichever field this version uses. A loaded-but-
    // paused track must not read as playing, so a missing field is only
    // treated as playing when there is nothing better -- confirmed against
    // real output before this is trusted.
    let playing = p
        .get("playing")
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            p.get("playbackRate")
                .and_then(serde_json::Value::as_f64)
                .map(|r| r > 0.0)
        })
        .unwrap_or(true);
    Some(Some(Now {
        app,
        title,
        artist: s("artist"),
        album: s("album"),
        playing,
        // artworkData is deliberately never read: unvelt stores no cover art.
    }))
}

/// The `media-control` binary, or `None` if it is not installed.
///
/// A GUI app does not inherit the shell PATH, so a bare-name lookup would fail
/// even with the tool installed. The two Homebrew prefixes are checked
/// directly instead -- Apple Silicon first, then Intel.
#[cfg(target_os = "macos")]
fn media_control_bin() -> Option<String> {
    [
        "/opt/homebrew/bin/media-control",
        "/usr/local/bin/media-control",
    ]
    .into_iter()
    .find(|p| std::path::Path::new(p).exists())
    .map(str::to_string)
}

#[cfg(target_os = "macos")]
fn applescript_now() -> Result<Option<Now>, String> {
    const SCRIPT: &str = r#"on q(a)
  tell application "System Events"
    if not (exists process a) then return ""
  end tell
  tell application a
    if it is not running then return ""
    try
      set s to (player state as text)
    on error
      return ""
    end try
    if s is not "playing" and s is not "paused" then return ""
    try
      return a & "<<>>" & s & "<<>>" & (name of current track) & "<<>>" & (artist of current track) & "<<>>" & (album of current track)
    on error
      return a & "<<>>" & s & "<<>>" & "" & "<<>>" & "" & "<<>>" & ""
    end try
  end tell
end q
set r to q("Spotify")
if r is "" then set r to q("Music")
return r"#;

    let out = crate::backend::run("osascript", &["-e", SCRIPT]);
    let out = out.trim();
    if out.is_empty() {
        return Ok(None);
    }
    let p: Vec<&str> = out.split("<<>>").collect();
    let get = |i: usize| p.get(i).map(|s| s.trim().to_string()).unwrap_or_default();
    Ok(Some(Now {
        // The bundle id namespace `desktop.focus` uses, so one app has one key.
        app: match get(0).as_str() {
            "Spotify" => "com.spotify.client".into(),
            "Music" => "com.apple.Music".into(),
            other => other.to_string(),
        },
        playing: get(1) == "playing",
        title: get(2),
        artist: get(3),
        album: get(4),
    }))
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn current() -> Result<Option<Now>, String> {
    // MPRIS2 over D-Bus belongs here and is its own change. Returning "nothing
    // playing" is the honest interim answer rather than pretending the
    // platform said so.
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_control_get_top_level_shape() {
        // What `media-control get` emits: the payload at the top level. A
        // browser track is the whole point -- Spotify already has an
        // AppleScript path, and this is the signal that one cannot reach.
        let out = r#"{"bundleIdentifier":"com.google.Chrome","title":"Some Song",
            "artist":"An Artist","album":"An Album","playing":true,
            "artworkData":"iVBORw0KGgoAAAA=="}"#;
        let now = parse_media_control(out).unwrap().unwrap();
        assert_eq!(now.app, "com.google.Chrome");
        assert_eq!(now.title, "Some Song");
        assert_eq!(now.artist, "An Artist");
        assert_eq!(now.album, "An Album");
        assert!(now.playing);
    }

    #[test]
    fn media_control_stream_payload_wrapper_and_playback_rate() {
        // The `stream` shape wraps the same fields under `payload`, and some
        // versions carry play state as a rate rather than a bool. Both are
        // tolerated so a format the field research could not pin down does not
        // silently collect nothing.
        let out = r#"{"diff":false,"payload":{"bundleIdentifier":"com.apple.Safari",
            "title":"Paused Track","artist":"X","playbackRate":0.0}}"#;
        let now = parse_media_control(out).unwrap().unwrap();
        assert_eq!(now.app, "com.apple.Safari");
        assert!(!now.playing, "playbackRate 0 must read as paused");
    }

    #[test]
    fn media_control_nothing_playing_is_not_a_fallback() {
        // Empty output and an empty payload both mean "the tool answered,
        // nothing is playing" -- Some(None). Only genuinely unparseable output
        // returns None, which is the sole trigger to fall back to AppleScript.
        assert_eq!(parse_media_control(""), Some(None));
        assert_eq!(parse_media_control("{}"), Some(None));
        assert_eq!(
            parse_media_control(r#"{"title":"","bundleIdentifier":""}"#),
            Some(None)
        );
        assert_eq!(parse_media_control("not json at all"), None);
        assert_eq!(parse_media_control("<html>error</html>"), None);
    }

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
