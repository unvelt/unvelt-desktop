//! OS primitives, one implementation per platform.
//!
//! Every method returns a plain value or `None` when the platform cannot answer
//! it. Handlers treat `None` as "no signal" and stay quiet — which is what
//! keeps a probe that a given OS simply does not support from turning into a
//! stream of zeroes that look like real observations. Nothing here panics; a
//! failing probe yields `None`.
//!
//! Ported from `compound/backends.py`. The Windows implementation is native
//! Win32 throughout. macOS and Linux keep the subprocess probes the Python
//! collector uses, because replacing those with native frameworks is a change
//! in behaviour as well as in language, and this port's whole acceptance test
//! is that behaviour did not change. They are marked individually below.

use crate::config::Config;

pub struct Power {
    pub ac: bool,
    pub pct: Option<u8>,
}

/// What the frontmost window is, in the terms the registry uses.
///
/// `pkg` is the platform-native identifier and `label` the human name, and they
/// are separate fields because conflating them is the bug migration 0015 exists
/// to fix: Chrome is `chrome` on Windows, `com.google.Chrome` on macOS and
/// `com.android.chrome` on the phone, and only the label folds those into one
/// app. A collector that sends the label as the key forks `dim_key` forever.
pub struct Front {
    pub pkg: String,
    pub label: Option<String>,
    pub title: String,
}

pub trait Backend {
    fn frontmost(&self) -> Option<Front>;
    /// Seconds since the last keyboard or mouse input.
    fn idle(&self) -> f64;
    fn fullscreen(&self) -> Option<bool>;
    fn locked(&self) -> Option<bool>;
    fn ssid(&mut self) -> Option<String>;
    fn monitors(&mut self) -> Option<i32>;
    fn net(&mut self) -> Option<String>;
    fn power(&self) -> Option<Power>;
}

/// Foreground apps whose window title is set by whatever runs inside them. When
/// that title is empty we fall back to the deepest child command — the thing
/// actually running in the terminal — which is a far better signal than a blank.
pub const TERMS: &[&str] = &[
    "alacritty",
    "windowsterminal",
    "wt",
    "cmd",
    "powershell",
    "pwsh",
    "conhost",
    "wezterm",
    "kitty",
    "iterm2",
    "terminal",
    "tmux",
];

/// Processes to skip when picking the "real" command inside a terminal.
pub const SHELLS: &[&str] = &[
    "conhost",
    "conemu",
    "conemuc",
    "login",
    "zsh",
    "-zsh",
    "bash",
    "-bash",
    "sh",
    "fish",
    "pwsh",
    "powershell",
    "cmd",
];

/// A tiny TTL cache, so slow probes (`system_profiler`, `netsh`) do not run on
/// every five-second tick.
pub struct Cached<T> {
    ttl: std::time::Duration,
    at: Option<std::time::Instant>,
    value: Option<T>,
}

impl<T: Clone> Cached<T> {
    pub fn new(ttl_secs: u64) -> Self {
        Cached {
            ttl: std::time::Duration::from_secs(ttl_secs),
            at: None,
            value: None,
        }
    }

    pub fn get(&mut self, f: impl FnOnce() -> Option<T>) -> Option<T> {
        let fresh = self.at.map(|t| t.elapsed() < self.ttl).unwrap_or(false);
        if !fresh {
            self.value = f();
            self.at = Some(std::time::Instant::now());
        }
        self.value.clone()
    }
}

/// Run a helper process and return its stdout, or an empty string.
///
/// On Windows a console child launched from a windowless agent pops a console
/// window that flashes on screen -- `netsh`, `ipconfig` and `arp` all do it.
/// CREATE_NO_WINDOW suppresses that; the flag does not exist elsewhere.
pub fn run(prog: &str, args: &[&str]) -> String {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(prog);
    cmd.args(args).stdin(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// The raw bytes of one REG_BINARY value under HKEY_CURRENT_USER.
///
/// Only `ambient` needs this, for the one Windows fact that has no API.
#[cfg(windows)]
pub fn reg_binary(path: &str, name: &str) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    };
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    unsafe {
        let mut h: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide(path).as_ptr(), 0, KEY_READ, &mut h)
            != ERROR_SUCCESS
        {
            return None;
        }
        let key = wide(name);
        let mut size = 0u32;
        let mut st = RegQueryValueExW(
            h,
            key.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        );
        if st != ERROR_SUCCESS || size == 0 {
            RegCloseKey(h);
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        st = RegQueryValueExW(
            h,
            key.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            &mut size,
        );
        RegCloseKey(h);
        (st == ERROR_SUCCESS).then_some(buf)
    }
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::WindowsBackend;

#[cfg(target_os = "macos")]
pub mod mac_av;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::UnixBackend;

pub fn make_backend(cfg: &Config) -> Box<dyn Backend> {
    #[cfg(windows)]
    {
        Box::new(WindowsBackend::new(cfg))
    }
    #[cfg(unix)]
    {
        Box::new(UnixBackend::new(cfg))
    }
}
