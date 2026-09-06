//! Core Audio and CoreMediaIO, for the questions macOS will only answer here.
//!
//! Three signals come out of this one file because they come out of one API
//! family, and building it once serves all of them:
//!
//!   * which processes are producing sound  -> `desktop.playing`
//!   * which processes are capturing input  -> `desktop.capture` (mic)
//!   * whether any camera is running        -> `desktop.capture` (cam)
//!
//! ALL PUBLIC, NO PROMPT, NO ENTITLEMENT
//!
//! Every property below is documented on a public framework. Reading them
//! triggers no TCC prompt and needs no entitlement, which is exactly why the
//! switches in the app default to off: when the platform declines to ask on
//! our behalf, the app's own switch is the only place the question gets put.
//!
//! AN ASYMMETRY WORTH KNOWING BEFORE READING THE DATA
//!
//! Core Audio can name the *process* using the microphone (macOS 14.2+).
//! CoreMediaIO cannot: it exposes whether a camera is running and nothing
//! about who is running it. So a Mac reports `cam` with no app while Windows
//! names one. That is a real difference in what the two platforms will tell
//! us, not a gap in this code, and the payload says so by omitting `app`
//! rather than inventing one.

#![cfg(target_os = "macos")]

use std::collections::BTreeSet;

type OSStatus = i32;
type AudioObjectID = u32;

const SYSTEM_OBJECT: AudioObjectID = 1;

/// Selectors and scopes are FourCC codes: four ASCII bytes as a big-endian
/// u32. Spelling them from the literal keeps them checkable against Apple's
/// headers by eye.
const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const ELEMENT_MAIN: u32 = 0;

const PROP_PROCESS_LIST: u32 = fourcc(b"prol"); // kAudioHardwarePropertyProcessObjectList
const PROP_IS_RUNNING_INPUT: u32 = fourcc(b"piri"); // kAudioProcessPropertyIsRunningInput
const PROP_IS_RUNNING_OUTPUT: u32 = fourcc(b"piro"); // kAudioProcessPropertyIsRunningOutput
const PROP_BUNDLE_ID: u32 = fourcc(b"pbid"); // kAudioProcessPropertyBundleID
const PROP_PID: u32 = fourcc(b"ppid"); // kAudioProcessPropertyPID
const PROP_DEFAULT_OUTPUT: u32 = fourcc(b"dOut"); // kAudioHardwarePropertyDefaultOutputDevice
const PROP_OBJECT_NAME: u32 = fourcc(b"lnam"); // kAudioObjectPropertyName
const PROP_TRANSPORT_TYPE: u32 = fourcc(b"tran"); // kAudioDevicePropertyTransportType

// CoreMediaIO mirrors the Core Audio shapes with its own object space.
const CMIO_PROP_DEVICES: u32 = fourcc(b"dev#"); // kCMIOHardwarePropertyDevices
const CMIO_PROP_RUNNING_SOMEWHERE: u32 = fourcc(b"gone"); // kCMIODevicePropertyDeviceIsRunningSomewhere

#[repr(C)]
#[derive(Clone, Copy)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

impl PropertyAddress {
    const fn global(selector: u32) -> Self {
        PropertyAddress {
            selector,
            scope: SCOPE_GLOBAL,
            element: ELEMENT_MAIN,
        }
    }
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyDataSize(
        id: AudioObjectID,
        addr: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const std::ffi::c_void,
        out_size: *mut u32,
    ) -> OSStatus;

    fn AudioObjectGetPropertyData(
        id: AudioObjectID,
        addr: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const std::ffi::c_void,
        io_size: *mut u32,
        out_data: *mut std::ffi::c_void,
    ) -> OSStatus;
}

#[link(name = "CoreMediaIO", kind = "framework")]
extern "C" {
    fn CMIOObjectGetPropertyDataSize(
        id: u32,
        addr: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const std::ffi::c_void,
        out_size: *mut u32,
    ) -> OSStatus;

    fn CMIOObjectGetPropertyData(
        id: u32,
        addr: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const std::ffi::c_void,
        in_size: u32,
        out_used: *mut u32,
        out_data: *mut std::ffi::c_void,
    ) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringGetCString(
        s: *const std::ffi::c_void,
        buf: *mut i8,
        size: isize,
        encoding: u32,
    ) -> u8;
    fn CFRelease(cf: *const std::ffi::c_void);
}

const KCFSTRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// A `CFStringRef` we own, rendered and released.
///
/// Every one of these comes from a "Get...PropertyData" call that follows the
/// Create Rule, so not releasing it leaks a little on every poll -- and this
/// polls forever.
unsafe fn take_cfstring(s: *const std::ffi::c_void) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let mut buf = [0i8; 512];
    let ok = CFStringGetCString(
        s,
        buf.as_mut_ptr(),
        buf.len() as isize,
        KCFSTRING_ENCODING_UTF8,
    );
    CFRelease(s);
    if ok == 0 {
        return None;
    }
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8(bytes).ok().filter(|s| !s.is_empty())
}

fn audio_array<T: Copy + Default>(id: AudioObjectID, selector: u32) -> Vec<T> {
    unsafe {
        let addr = PropertyAddress::global(selector);
        let mut size = 0u32;
        if AudioObjectGetPropertyDataSize(id, &addr, 0, std::ptr::null(), &mut size) != 0 {
            return Vec::new();
        }
        let n = size as usize / std::mem::size_of::<T>();
        if n == 0 {
            return Vec::new();
        }
        let mut out = vec![T::default(); n];
        if AudioObjectGetPropertyData(
            id,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            out.as_mut_ptr() as *mut _,
        ) != 0
        {
            return Vec::new();
        }
        out
    }
}

fn audio_u32(id: AudioObjectID, selector: u32) -> Option<u32> {
    unsafe {
        let addr = PropertyAddress::global(selector);
        let mut v = 0u32;
        let mut size = 4u32;
        let st = AudioObjectGetPropertyData(
            id,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut v as *mut _ as *mut _,
        );
        (st == 0).then_some(v)
    }
}

fn audio_string(id: AudioObjectID, selector: u32) -> Option<String> {
    unsafe {
        let addr = PropertyAddress::global(selector);
        let mut s: *const std::ffi::c_void = std::ptr::null();
        let mut size = std::mem::size_of::<*const std::ffi::c_void>() as u32;
        let st = AudioObjectGetPropertyData(
            id,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut s as *mut _ as *mut _,
        );
        (st == 0).then(|| take_cfstring(s)).flatten()
    }
}

/// The bundle id of a process object, or its pid rendered as a name when the
/// process has no bundle -- a bare unix binary run from a terminal.
fn process_name(obj: AudioObjectID) -> Option<String> {
    if let Some(b) = audio_string(obj, PROP_BUNDLE_ID) {
        return Some(b);
    }
    audio_u32(obj, PROP_PID).map(|pid| format!("pid:{pid}"))
}

/// Bundle ids of processes currently producing sound.
///
/// The same namespace `desktop.focus` sends on macOS -- bundle ids -- so one
/// app has one key across both, which is what migration 0015 keys app labels
/// on.
pub fn audible_apps() -> BTreeSet<String> {
    running(PROP_IS_RUNNING_OUTPUT)
}

/// Bundle ids of processes currently capturing from an input device.
pub fn recording_apps() -> BTreeSet<String> {
    running(PROP_IS_RUNNING_INPUT)
}

fn running(prop: u32) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for obj in audio_array::<AudioObjectID>(SYSTEM_OBJECT, PROP_PROCESS_LIST) {
        // macOS 14.2 and later. On anything older the property is absent, the
        // call fails, and this yields nothing -- which is the honest answer
        // for a system that cannot tell us.
        if audio_u32(obj, prop).unwrap_or(0) == 0 {
            continue;
        }
        if let Some(name) = process_name(obj) {
            out.insert(name);
        }
    }
    out
}

/// Whether any camera is currently running, anywhere on the system.
///
/// `None` when CoreMediaIO will not answer. Deliberately not `Some(false)`:
/// "no camera is on" and "we cannot see cameras" are different claims and only
/// one of them belongs in the data.
pub fn camera_running() -> Option<bool> {
    unsafe {
        let addr = PropertyAddress::global(CMIO_PROP_DEVICES);
        let mut size = 0u32;
        if CMIOObjectGetPropertyDataSize(SYSTEM_OBJECT, &addr, 0, std::ptr::null(), &mut size) != 0
        {
            return None;
        }
        let n = size as usize / std::mem::size_of::<u32>();
        if n == 0 {
            // No capture devices at all is a real answer: nothing is running.
            return Some(false);
        }
        let mut devices = vec![0u32; n];
        let mut used = 0u32;
        if CMIOObjectGetPropertyData(
            SYSTEM_OBJECT,
            &addr,
            0,
            std::ptr::null(),
            size,
            &mut used,
            devices.as_mut_ptr() as *mut _,
        ) != 0
        {
            return None;
        }
        let mut any = false;
        let running = PropertyAddress::global(CMIO_PROP_RUNNING_SOMEWHERE);
        for dev in devices {
            let mut v = 0u32;
            let mut vsize = 4u32;
            let st = CMIOObjectGetPropertyData(
                dev,
                &running,
                0,
                std::ptr::null(),
                4,
                &mut vsize,
                &mut v as *mut _ as *mut _,
            );
            if st == 0 && v != 0 {
                any = true;
                break;
            }
        }
        Some(any)
    }
}

/// The default output device, as a route and a name.
///
/// Headphones on is a different kind of listening from a speaker in a shared
/// room, and it is the cheapest available proxy for whether somebody is alone.
pub fn output_route() -> Option<(&'static str, Option<String>)> {
    let dev = audio_u32(SYSTEM_OBJECT, PROP_DEFAULT_OUTPUT)?;
    if dev == 0 {
        return None;
    }
    let name = audio_string(dev, PROP_OBJECT_NAME);
    // Transport type is what actually distinguishes a route; the device name
    // is a label people choose and cannot be classified on.
    let route = match audio_u32(dev, PROP_TRANSPORT_TYPE).map(u32::to_be_bytes) {
        Some(t) if &t == b"blth" => "headphones", // Bluetooth: earbuds or a speaker
        Some(t) if &t == b"hdpn" => "headphones",
        Some(t) if &t == b"usb " => "external",
        Some(t) if &t == b"hdmi" || matches!(&t, b"dprt") => "external",
        Some(t) if &t == b"bltn" => "speakers", // built-in
        _ => "speakers",
    };
    Some((route, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_packs_the_way_apple_writes_it() {
        // 'glob' is 0x676C6F62. Getting the byte order wrong would make every
        // property lookup fail silently and this file would report nothing,
        // forever, on a machine that had plenty to say.
        assert_eq!(fourcc(b"glob"), 0x676C_6F62);
        assert_eq!(fourcc(b"prol"), 0x7072_6F6C);
        assert_eq!(fourcc(b"gone"), 0x676F_6E65);
    }

    #[test]
    fn the_machine_answers_without_lying() {
        // Bounds, not values: what is playing on a CI runner is nobody's
        // business to assert. This catches a signature that compiles and
        // returns garbage.
        assert!(audible_apps().len() < 128);
        assert!(recording_apps().len() < 128);
        if let Some((route, _)) = output_route() {
            assert!(["headphones", "speakers", "external"].contains(&route));
        }
    }
}
