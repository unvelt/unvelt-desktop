//! macOS and Linux probes.
//!
//! These keep the subprocess approach the Python collector uses, and that is a
//! deliberate scope line rather than laziness: the acceptance test for this
//! port is that a week of side-by-side capture produces the same events, and a
//! probe rewritten onto a native framework at the same time as the language
//! changed would make every difference ambiguous. Native Core Audio, the
//! `usernoted` read and the rest arrive with the signals that need them.
//!
//! One thing does change, and it is the fix migration 0015 was written for.
//! `frontmost()` on macOS used to send the display name — "Google Chrome" — as
//! the app key, so the same browser was `Google Chrome` from a Mac and
//! `com.android.chrome` from the phone and the two never folded. It now sends
//! the **bundle id** as `pkg` and the display name as `label`, and the label
//! is carried into `inventory.app` so breakdowns still read "Google Chrome".
//! Windows was never affected: it has always sent the executable name.

use super::{Backend, Cached, Front, Power, SHELLS, TERMS};
use crate::backend::run;
use crate::config::Config;

/// Ask System Events for the frontmost process, its bundle id, and the title of
/// the window that actually has focus.
///
/// Alacritty and other winit/GPU-backed apps sort several nameless AXUnknown
/// helper windows ahead of the real one, so "front window" is a phantom with no
/// name and the title came back empty. Ask for the focused window by attribute
/// first, then fall back to the front window, then to the first genuine
/// AXStandardWindow. Each step is guarded so one quirky app cannot blank out
/// the process name as well.
const FRONT_SCRIPT: &str = r#"tell application "System Events"
  set p to first application process whose frontmost is true
  set a to name of p
  set b to ""
  try
    set b to bundle identifier of p
  end try
  if b is missing value then set b to ""
  set u to unix id of p
  set w to ""
  try
    set w to (value of attribute "AXTitle" of (value of attribute "AXFocusedWindow" of p))
  end try
  if w is missing value then set w to ""
  if w is "" then
    try
      set w to name of front window of p
    end try
  end if
  if w is missing value then set w to ""
  if w is "" then
    try
      set w to name of first window of p whose subrole is "AXStandardWindow"
    end try
  end if
  if w is missing value then set w to ""
end tell
return a & "<<>>" & b & "<<>>" & w & "<<>>" & (u as text)"#;

pub struct UnixBackend {
    ssid: Cached<String>,
    net: Cached<String>,
    monitors: Cached<i32>,
}

impl UnixBackend {
    pub fn new(cfg: &Config) -> Self {
        UnixBackend {
            // The macOS SSID path can fall through to a ~4s system_profiler
            // call, so it gets its own much slower clock.
            ssid: Cached::new(cfg.ssid_sec),
            net: Cached::new(cfg.context_sec),
            monitors: Cached::new(cfg.context_sec),
        }
    }
}

/// The deepest non-shell descendant of `pid` — the command running in a
/// terminal. Empty when there is none.
fn child_cmd(pid: i64) -> String {
    let out = run("ps", &["-axo", "pid=,ppid=,comm="]);
    let mut kids: std::collections::HashMap<i64, Vec<(i64, String)>> = Default::default();
    for line in out.lines() {
        let mut it = line.split_whitespace();
        let (Some(cpid), Some(ppid)) = (it.next(), it.next()) else {
            continue;
        };
        let comm: String = it.collect::<Vec<_>>().join(" ");
        let (Ok(cpid), Ok(ppid)) = (cpid.parse::<i64>(), ppid.parse::<i64>()) else {
            continue;
        };
        let name = comm.rsplit('/').next().unwrap_or(&comm).trim().to_string();
        if !name.is_empty() {
            kids.entry(ppid).or_default().push((cpid, name));
        }
    }
    let mut best: (Option<String>, i32) = (None, -1);
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![(pid, 0i32)];
    while let Some((p, depth)) = stack.pop() {
        if !seen.insert(p) {
            continue;
        }
        for (cpid, name) in kids.get(&p).into_iter().flatten() {
            if !SHELLS.contains(&name.to_ascii_lowercase().as_str()) && depth > best.1 {
                best = (Some(name.clone()), depth);
            }
            stack.push((*cpid, depth + 1));
        }
    }
    best.0.unwrap_or_default()
}

/// Modern macOS returns the literal "<redacted>" for the SSID when the caller
/// has no Location permission. Treat that as no answer, so the next method —
/// which may not be gated — gets a turn.
fn usable_ssid(name: &str) -> bool {
    !name.is_empty() && !name.to_ascii_lowercase().contains("redacted")
}

impl Backend for UnixBackend {
    fn frontmost(&self) -> Option<Front> {
        if cfg!(target_os = "macos") {
            let out = run("osascript", &["-e", FRONT_SCRIPT]);
            if out.trim().is_empty() {
                return None;
            }
            let parts: Vec<&str> = out.trim().split("<<>>").collect();
            let label = parts
                .first()
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let bundle = parts
                .get(1)
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let mut title = parts
                .get(2)
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let upid = parts.get(3).and_then(|s| s.trim().parse::<i64>().ok());

            // The bundle id is the key; the display name is only a label. When
            // an app has no bundle id at all -- a bare unix binary run from a
            // terminal -- the display name is the only identifier there is, so
            // it becomes the key and is NOT also sent as the label, or
            // "resolved" and "unresolved" stop being distinguishable.
            let (pkg, label) = if bundle.is_empty() {
                (label, None)
            } else {
                (bundle, if label.is_empty() { None } else { Some(label) })
            };
            if title.is_empty() && TERMS.contains(&pkg.to_ascii_lowercase().as_str()) {
                if let Some(pid) = upid {
                    title = child_cmd(pid);
                }
            }
            if pkg.is_empty() && title.is_empty() {
                return None;
            }
            return Some(Front { pkg, label, title });
        }

        // Linux, X11 only for now. Wayland deliberately gives no cross-desktop
        // way to ask what has focus, and guessing per-compositor belongs in its
        // own change rather than smuggled into a port.
        let id = run("xdotool", &["getactivewindow"]).trim().to_string();
        if id.is_empty() {
            return None;
        }
        let title = run("xdotool", &["getwindowname", &id]).trim().to_string();
        let pid = run("xdotool", &["getwindowpid", &id])
            .trim()
            .parse::<i64>()
            .ok();
        let pkg = pid
            .map(|p| {
                run("ps", &["-p", &p.to_string(), "-o", "comm="])
                    .trim()
                    .to_string()
            })
            .unwrap_or_default();
        if pkg.is_empty() && title.is_empty() {
            return None;
        }
        Some(Front {
            pkg,
            label: None,
            title,
        })
    }

    fn idle(&self) -> f64 {
        if cfg!(target_os = "macos") {
            let out = run("ioreg", &["-c", "IOHIDSystem"]);
            for line in out.lines() {
                if line.contains("HIDIdleTime") {
                    if let Some(v) = line.rsplit('=').next() {
                        if let Ok(ns) = v.trim().trim_matches('"').parse::<f64>() {
                            return ns / 1e9;
                        }
                    }
                    return 0.0;
                }
            }
            return 0.0;
        }
        let out = run("xprintidle", &[]);
        out.trim()
            .parse::<f64>()
            .map(|ms| ms / 1000.0)
            .unwrap_or(0.0)
    }

    fn fullscreen(&self) -> Option<bool> {
        None // not probed on Unix in v1, same as the Python collector
    }

    fn locked(&self) -> Option<bool> {
        None // handled by idle in v1; the controller still sees sleep via the clock
    }

    fn ssid(&mut self) -> Option<String> {
        self.ssid.get(|| {
            if !cfg!(target_os = "macos") {
                let out = run("iwgetid", &["-r"]);
                let name = out.trim();
                return usable_ssid(name).then(|| name.to_string());
            }
            let iface = wifi_iface().unwrap_or_else(|| "en0".into());

            // 1) networksetup: works pre-Sonoma, blank or redacted after it.
            let out = run("networksetup", &["-getairportnetwork", &iface]);
            let low = out.to_ascii_lowercase();
            if out.contains(':')
                && !low.contains("not associated")
                && !low.contains("not a wi-fi")
                && !low.contains("not a wifi")
            {
                let name = out.splitn(2, ':').nth(1).unwrap_or("").trim();
                if usable_ssid(name) {
                    return Some(name.to_string());
                }
            }

            // 2) ipconfig getsummary: the common Sonoma+ route, and no prompt.
            let out = run("ipconfig", &["getsummary", &iface]);
            for line in out.lines() {
                let t = line.trim();
                if let Some(rest) = t.strip_prefix("SSID") {
                    let v = rest.trim_start_matches([':', '=', ' ']).trim();
                    if usable_ssid(v) {
                        return Some(v.to_string());
                    }
                }
            }

            // 3) system_profiler: slow, last resort.
            let out = run("system_profiler", &["SPAirPortDataType"]);
            let lines: Vec<&str> = out.lines().collect();
            for (i, ln) in lines.iter().enumerate() {
                if ln.contains("Current Network Information") {
                    for nxt in lines.iter().skip(i + 1).take(3) {
                        let s = nxt.trim();
                        if let Some(name) = s.strip_suffix(':') {
                            if usable_ssid(name.trim()) {
                                return Some(name.trim().to_string());
                            }
                        }
                    }
                }
            }
            None
        })
    }

    fn monitors(&mut self) -> Option<i32> {
        self.monitors.get(|| {
            if cfg!(target_os = "macos") {
                let out = run("system_profiler", &["SPDisplaysDataType"]);
                let n = out.matches("Resolution:").count() as i32;
                return (n > 0).then_some(n);
            }
            let out = run("xrandr", &["--listmonitors"]);
            let n = out
                .lines()
                .filter(|l| l.trim_start().starts_with(char::is_numeric))
                .count() as i32;
            (n > 0).then_some(n)
        })
    }

    fn net(&mut self) -> Option<String> {
        self.net.get(|| {
            // Gateway MAC: a stable per-network id that Location does NOT gate,
            // so it still answers when the SSID comes back redacted.
            let gw = if cfg!(target_os = "macos") {
                run("route", &["-n", "get", "default"])
                    .lines()
                    .find_map(|l| {
                        l.trim()
                            .strip_prefix("gateway:")
                            .map(|v| v.trim().to_string())
                    })
            } else {
                run("ip", &["route", "show", "default"])
                    .split_whitespace()
                    .nth(2)
                    .map(|s| s.to_string())
            }?;
            let out = if cfg!(target_os = "macos") {
                run("arp", &["-n", &gw])
            } else {
                run("ip", &["neigh", "show", &gw])
            };
            out.split_whitespace()
                .find(|tok| {
                    let p: Vec<&str> = tok.split(':').collect();
                    p.len() == 6
                        && p.iter()
                            .all(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_hexdigit()))
                })
                .map(|m| m.to_ascii_lowercase())
        })
    }

    fn power(&self) -> Option<Power> {
        if cfg!(target_os = "macos") {
            let out = run("pmset", &["-g", "batt"]);
            if out.trim().is_empty() {
                return None;
            }
            let ac = out.contains("AC Power");
            let pct = out.split('%').next().and_then(|head| {
                let digits: String = head
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                digits.parse::<u8>().ok()
            });
            return Some(Power { ac, pct });
        }
        let base = std::path::Path::new("/sys/class/power_supply");
        let mut pct = None;
        let mut ac = None;
        for e in std::fs::read_dir(base).ok()?.flatten() {
            let p = e.path();
            let kind = std::fs::read_to_string(p.join("type")).unwrap_or_default();
            match kind.trim() {
                "Battery" if pct.is_none() => {
                    pct = std::fs::read_to_string(p.join("capacity"))
                        .ok()
                        .and_then(|v| v.trim().parse::<u8>().ok());
                }
                "Mains" if ac.is_none() => {
                    ac = std::fs::read_to_string(p.join("online"))
                        .ok()
                        .map(|v| v.trim() == "1");
                }
                _ => {}
            }
        }
        // A desktop with no battery and no mains node tells us nothing, and
        // "nothing" must not become "on battery at 0%".
        (pct.is_some() || ac.is_some()).then(|| Power {
            ac: ac.unwrap_or(true),
            pct,
        })
    }
}

/// The real Wi-Fi device, which is en0 or en1 depending on the Mac.
fn wifi_iface() -> Option<String> {
    let out = run("networksetup", &["-listallhardwareports"]);
    let lines: Vec<&str> = out.lines().collect();
    for (i, ln) in lines.iter().enumerate() {
        if ln.starts_with("Hardware Port:") && (ln.contains("Wi-Fi") || ln.contains("AirPort")) {
            for nxt in lines.iter().skip(i + 1).take(2) {
                if let Some(dev) = nxt.strip_prefix("Device:") {
                    return Some(dev.trim().to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_ssid_is_not_an_answer() {
        assert!(usable_ssid("HomeNet"));
        assert!(!usable_ssid(""));
        assert!(!usable_ssid("<redacted>"));
        assert!(!usable_ssid("<REDACTED>"));
    }
}
