//! One collector per user, per machine.
//!
//! Without this, launching the app twice — or the app and the CLI together —
//! gives two loops polling the same OS and posting under the same device id.
//! They do not dedupe: `eid` is keyed on the emit timestamp in milliseconds,
//! and two independent loops will not land on the same millisecond, so every
//! signal is simply counted twice. Screen time doubles, `desktop.input`
//! reports a machine used twice as hard as it was, and nothing in the data
//! says why.
//!
//! Scoped to the login session, not the whole machine: two people signed in to
//! the same PC are two participants, and each is entitled to their own
//! collector.
//!
//! `--probe`, `--login` and `--once` do not take the lock. The first two do not
//! collect at all, and `--once` is the parity harness, where a single
//! deliberate extra cycle is the entire point.

/// Held for as long as this process should be the only collector. Dropping it
/// releases the claim; the OS releases it anyway if the process dies, which is
/// what makes this safe after a crash where a plain lock file would not be.
pub struct Instance {
    #[cfg(windows)]
    _handle: WinHandle,
    #[cfg(unix)]
    _file: std::fs::File,
}

#[cfg(windows)]
struct WinHandle(windows_sys::Win32::Foundation::HANDLE);

// The mutex is owned by this process and released by the kernel on exit. The
// raw handle is only ever closed on drop, from the thread that made it.
#[cfg(windows)]
unsafe impl Send for WinHandle {}

#[cfg(windows)]
impl Drop for WinHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// `None` when another collector already holds the claim.
#[cfg(windows)]
pub fn acquire() -> Option<Instance> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    // "Local\" scopes the name to the login session, which is the boundary we
    // want: two users on one PC are two participants.
    let name: Vec<u16> = "Local\\unvelt-collector"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if h.is_null() {
            // Cannot tell whether we are alone. Collecting is the safer
            // failure: a duplicate is a data problem we can see and fix,
            // silence is one we cannot.
            return Some(Instance {
                _handle: WinHandle(std::ptr::null_mut()),
            });
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            windows_sys::Win32::Foundation::CloseHandle(h);
            return None;
        }
        Some(Instance {
            _handle: WinHandle(h),
        })
    }
}

#[cfg(unix)]
pub fn acquire() -> Option<Instance> {
    use std::os::unix::io::AsRawFd;

    let path = crate::config::state_dir().join("collector.lock");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ok()?;

    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    // flock, not a pid file: the kernel drops it when the process dies, so a
    // crash cannot leave a stale claim that keeps the collector off forever.
    let ok = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0;
    ok.then_some(Instance { _file: file })
}
