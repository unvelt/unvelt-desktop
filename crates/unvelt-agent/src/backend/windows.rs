//! Windows probes, native Win32 throughout.
//!
//! `pkg` is the executable base name without its extension — `chrome`,
//! `Code`, `alacritty`. That is already what the Python collector sends, so
//! five thousand existing `desktop.focus` events keep the same `dim_key` and
//! nothing in the history forks. It is also genuinely platform-native: a
//! Windows AUMID would be a better key for Store apps, but it is unavailable
//! for most desktop processes and switching to it would be the identifier
//! change 0015 warns about, not a port.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, HWND, MAX_PATH, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, SM_CMONITORS,
};

use super::{Backend, Cached, Front, Power, SHELLS, TERMS};
use crate::config::Config;

const DESKTOP_READOBJECTS: u32 = 0x0001_0000;

pub struct WindowsBackend {
    ssid: Cached<String>,
    net: Cached<String>,
}

impl WindowsBackend {
    pub fn new(cfg: &Config) -> Self {
        WindowsBackend {
            ssid: Cached::new(cfg.context_sec),
            net: Cached::new(cfg.context_sec),
        }
    }
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..end])
        .to_string_lossy()
        .into_owned()
}

/// Executable base name without extension, for a pid. Empty on any failure.
fn proc_name(pid: u32) -> String {
    unsafe {
        let h: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if h.is_null() {
            return String::new();
        }
        let mut buf = [0u16; MAX_PATH as usize];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(h);
        if ok == 0 {
            return String::new();
        }
        let full = wide_to_string(&buf[..size as usize]);
        let file = full.rsplit(['\\', '/']).next().unwrap_or(&full);
        match file.rfind('.') {
            Some(i) => file[..i].to_string(),
            None => file.to_string(),
        }
    }
}

/// The deepest non-shell descendant of `pid` — the command actually running in
/// a terminal. Empty when there is none, or on any error.
fn child_cmd(pid: u32) -> String {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() {
            return String::new();
        }
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut kids: std::collections::HashMap<u32, Vec<(u32, String)>> = Default::default();
        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                let name = wide_to_string(&pe.szExeFile);
                let stem = match name.rfind('.') {
                    Some(i) => name[..i].to_string(),
                    None => name,
                };
                kids.entry(pe.th32ParentProcessID)
                    .or_default()
                    .push((pe.th32ProcessID, stem));
                if Process32NextW(snap, &mut pe) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);

        // Iterative rather than recursive: a process table can contain a cycle
        // after pid reuse, and a stack overflow in a collector is not a
        // tolerable way to find that out. `seen` bounds it either way.
        let mut best: (Option<String>, i32) = (None, -1);
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![(pid, 0i32)];
        while let Some((p, depth)) = stack.pop() {
            if !seen.insert(p) {
                continue;
            }
            for (cpid, cname) in kids.get(&p).into_iter().flatten() {
                if !SHELLS.contains(&cname.to_ascii_lowercase().as_str()) && depth > best.1 {
                    best = (Some(cname.clone()), depth);
                }
                stack.push((*cpid, depth + 1));
            }
        }
        best.0.unwrap_or_default()
    }
}

impl Backend for WindowsBackend {
    fn frontmost(&self) -> Option<Front> {
        unsafe {
            let hwnd: HWND = GetForegroundWindow();
            if hwnd.is_null() {
                return None;
            }
            let n = GetWindowTextLengthW(hwnd);
            let mut title = String::new();
            if n > 0 {
                let mut buf = vec![0u16; n as usize + 1];
                let got = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
                if got > 0 {
                    title = wide_to_string(&buf);
                }
            }
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let pkg = proc_name(pid);
            if title.is_empty() && TERMS.contains(&pkg.to_ascii_lowercase().as_str()) {
                title = child_cmd(pid);
            }
            if pkg.is_empty() && title.is_empty() {
                return None;
            }
            Some(Front {
                pkg,
                // Windows has no cheap display name for an arbitrary process.
                // FileDescription would need a version-resource read per app;
                // that belongs with the `inventory.app` work, not here, and
                // sending `None` is honest where sending the exe name twice
                // would make "resolved" and "unresolved" indistinguishable.
                label: None,
                title,
            })
        }
    }

    fn idle(&self) -> f64 {
        unsafe {
            let mut lii = LASTINPUTINFO {
                cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                dwTime: 0,
            };
            if GetLastInputInfo(&mut lii) == 0 {
                return 0.0;
            }
            // GetTickCount wraps every ~49.7 days and dwTime is on the same
            // clock, so a wrapping subtraction is correct across the rollover
            // where a plain one would report 49 days of idle for a moment.
            let ticks = GetTickCount().wrapping_sub(lii.dwTime);
            ticks as f64 / 1000.0
        }
    }

    fn fullscreen(&self) -> Option<bool> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                return None;
            }
            let mut r: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &mut r) == 0 {
                return None;
            }
            let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(hmon, &mut mi) == 0 {
                return None;
            }
            let m = mi.rcMonitor;
            Some(r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom)
        }
    }

    fn locked(&self) -> Option<bool> {
        unsafe {
            // The input desktop is unreachable while the secure desktop (the
            // lock screen) is in front, which is the cheapest reliable lock
            // check that needs no session notifications and no window.
            let h = OpenInputDesktop(0, FALSE, DESKTOP_READOBJECTS);
            if h.is_null() {
                return Some(true);
            }
            CloseDesktop(h);
            Some(false)
        }
    }

    fn ssid(&mut self) -> Option<String> {
        self.ssid.get(|| {
            let out = crate::backend::run("netsh", &["wlan", "show", "interfaces"]);
            for line in out.lines() {
                let t = line.trim();
                // "SSID" also prefixes "BSSID"; match the exact key.
                if let Some(rest) = t.strip_prefix("SSID") {
                    if let Some(v) = rest.trim_start().strip_prefix(':') {
                        let v = v.trim();
                        if !v.is_empty() {
                            return Some(v.to_string());
                        }
                    }
                }
            }
            None
        })
    }

    fn monitors(&mut self) -> Option<i32> {
        let n = unsafe { GetSystemMetrics(SM_CMONITORS) };
        if n > 0 {
            Some(n)
        } else {
            None
        }
    }

    fn net(&mut self) -> Option<String> {
        self.net.get(|| {
            // Gateway MAC: a stable per-network id (home vs office) that needs
            // no permission and survives an SSID the OS has started redacting.
            let out = crate::backend::run("ipconfig", &[]);
            let lines: Vec<&str> = out.lines().collect();
            let mut gw: Option<String> = None;
            for (i, ln) in lines.iter().enumerate() {
                if !ln.contains("Default Gateway") {
                    continue;
                }
                // The IPv4 gateway often sits on an unlabelled continuation
                // line below the IPv6 one, and an inactive adapter prints an
                // empty gateway -- so read the whole value block, not one line.
                let mut block = ln.to_string();
                for nxt in lines.iter().skip(i + 1) {
                    if nxt.trim().is_empty() || nxt.contains(':') {
                        break;
                    }
                    block.push(' ');
                    block.push_str(nxt);
                }
                if let Some(ip) = first_ipv4(&block) {
                    gw = Some(ip);
                    break;
                }
            }
            let gw = gw?;
            let out = crate::backend::run("arp", &["-a", &gw]);
            first_mac(&out).map(|m| m.replace('-', ":").to_ascii_lowercase())
        })
    }

    fn power(&self) -> Option<Power> {
        unsafe {
            let mut s: SYSTEM_POWER_STATUS = std::mem::zeroed();
            if GetSystemPowerStatus(&mut s) == 0 {
                return None;
            }
            // 255 means "unknown" for both fields, which is what a desktop with
            // no battery reports. Passing that through as 255% would be a lie
            // the digest cannot detect.
            let pct = if s.BatteryLifePercent <= 100 {
                Some(s.BatteryLifePercent)
            } else {
                None
            };
            Some(Power {
                ac: s.ACLineStatus == 1,
                pct,
            })
        }
    }
}

/// First dotted-quad in a string.
fn first_ipv4(s: &str) -> Option<String> {
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut parts = 0;
        let mut ok = true;
        while parts < 4 {
            let mut d = 0;
            while i < bytes.len() && bytes[i].is_ascii_digit() && d < 3 {
                i += 1;
                d += 1;
            }
            if d == 0 {
                ok = false;
                break;
            }
            parts += 1;
            if parts < 4 {
                if i < bytes.len() && bytes[i] == '.' {
                    i += 1;
                } else {
                    ok = false;
                    break;
                }
            }
        }
        if ok && parts == 4 {
            return Some(bytes[start..i].iter().collect());
        }
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == '.') {
            i += 1;
        }
    }
    None
}

/// First `xx-xx-xx-xx-xx-xx` or `xx:xx:...` MAC in a string.
fn first_mac(s: &str) -> Option<String> {
    for tok in s.split_whitespace() {
        let sep = if tok.contains('-') { '-' } else { ':' };
        let parts: Vec<&str> = tok.split(sep).collect();
        if parts.len() == 6
            && parts
                .iter()
                .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
        {
            return Some(tok.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_is_found_in_an_ipconfig_block() {
        let block = "   Default Gateway . . . . . . . . . : fe80::1%14\n                                       192.168.29.1";
        assert_eq!(first_ipv4(block).as_deref(), Some("192.168.29.1"));
        assert_eq!(first_ipv4("   Default Gateway . . . . . :"), None);
    }

    #[test]
    fn mac_is_found_in_arp_output() {
        let out = "Interface: 192.168.29.42 --- 0xe\n  Internet Address      Physical Address      Type\n  192.168.29.1          a4-2b-b0-11-22-33     dynamic";
        assert_eq!(first_mac(out).as_deref(), Some("a4-2b-b0-11-22-33"));
        assert_eq!(first_mac("no mac here 12-34"), None);
    }

    #[test]
    fn real_machine_answers_the_cheap_probes() {
        // Not a mock: these are the probes the whole agent rests on, and the
        // failure they guard against is a Win32 signature that compiles and
        // returns garbage. Bounds only -- exact values are the machine's.
        let cfg = Config::for_test();
        let mut b = WindowsBackend::new(&cfg);
        let idle = b.idle();
        assert!((0.0..86_400.0).contains(&idle), "implausible idle {idle}");
        let mons = b.monitors().unwrap_or(0);
        assert!((1..=16).contains(&mons), "implausible monitor count {mons}");
        assert!(b.locked().is_some(), "lock state is knowable on Windows");
    }
}
