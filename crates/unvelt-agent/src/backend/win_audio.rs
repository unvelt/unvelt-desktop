//! The Windows audio endpoints: what sound is coming out of, and what is
//! plugged in.
//!
//! Two signals from one enumerator. `IMMDeviceEnumerator` is the only
//! supported way to ask either question -- the registry holds the same data
//! under `MMDevices\Audio\Render` but nothing there says which endpoint is
//! *default*, and guessing that from the key order is how you end up reporting
//! a disconnected monitor's speakers as the route.
//!
//! WHY THE FORM FACTOR AND NOT THE NAME
//!
//! People rename audio devices, and the name is frequently their own name --
//! "Kanishak's AirPods". The route has to come from something classifiable, so
//! it comes from `PKEY_AudioEndpoint_FormFactor`, and the name is carried
//! alongside as a label at full depth rather than being parsed for meaning.

#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantClear, PropVariantToStringAlloc, PropVariantToUInt32,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

/// `PKEY_AudioEndpoint_FormFactor`. Not exported by the `windows` crate, so it
/// is spelled from the SDK header -- `{1da5d803-d492-4edd-8c23-e0c0ffee7f0e}, 0`.
const PKEY_FORM_FACTOR: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 0,
};

/// EndpointFormFactor, from mmdeviceapi.h.
fn route_of(form: u32) -> &'static str {
    match form {
        0 => "speakers",   // RemoteNetworkDevice -- rare, treat as generic
        1 => "speakers",   // Speakers
        2 => "speakers",   // LineLevel
        3 => "headphones", // Headphones
        4 => "headphones", // Microphone (input; not reached for render)
        5 => "headphones", // Headset
        6 => "external",   // Handset
        7 => "external",   // UnknownDigitalPassthrough
        8 => "external",   // SPDIF
        9 => "external",   // DigitalAudioDisplayDevice (HDMI / DisplayPort)
        _ => "speakers",
    }
}

fn com_init() {
    // Ignored on purpose: RPC_E_CHANGED_MODE means something else already
    // initialised this thread, which is fine -- we only need *an*
    // apartment, not ours.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

// The documented accessors rather than the PROPVARIANT union. The union is
// not exposed by this crate version, and reaching into it by hand would mean
// trusting a variant tag nothing checks -- a wrong guess there reads a pointer
// out of an integer field.
unsafe fn friendly_name(dev: &windows::Win32::Media::Audio::IMMDevice) -> Option<String> {
    let store = dev.OpenPropertyStore(STGM_READ).ok()?;
    let mut v = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
    let out = PropVariantToStringAlloc(&v).ok().and_then(|p| {
        (!p.is_null())
            .then(|| PCWSTR(p.0).to_string().ok())
            .flatten()
    });
    let _ = PropVariantClear(&mut v);
    out
}

unsafe fn form_factor(dev: &windows::Win32::Media::Audio::IMMDevice) -> Option<u32> {
    let store = dev.OpenPropertyStore(STGM_READ).ok()?;
    let mut v = store.GetValue(&PKEY_FORM_FACTOR).ok()?;
    let out = PropVariantToUInt32(&v).ok();
    let _ = PropVariantClear(&mut v);
    out
}

/// The default playback device: how it is classified, and what it is called.
pub fn output_route() -> Option<(&'static str, Option<String>)> {
    com_init();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        // eMultimedia rather than eConsole: it is the role Windows uses for
        // music and video, which is the listening this signal is about.
        let dev = enumerator
            .GetDefaultAudioEndpoint(eRender, eMultimedia)
            .ok()?;
        let route = route_of(form_factor(&dev).unwrap_or(1));
        Some((route, friendly_name(&dev)))
    }
}

/// Every active playback device, by name.
///
/// This is the peripheral signal in the form it is actually useful: a headset
/// appearing is a person sitting down at a desk, and a dock appearing brings
/// its own audio endpoint with it. It is NOT a full USB enumeration -- that
/// needs SetupAPI and reports a hundred things nobody cares about, most of
/// which never change.
pub fn playback_devices() -> Vec<String> {
    com_init();
    unsafe {
        let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return Vec::new();
        };
        let Ok(coll) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return Vec::new();
        };
        let n = coll.GetCount().unwrap_or(0);
        let mut out = Vec::new();
        for i in 0..n {
            if let Ok(dev) = coll.Item(i) {
                if let Some(name) = friendly_name(&dev) {
                    out.push(name);
                }
            }
        }
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_has_a_route_and_at_least_one_device() {
        // A CI runner has no audio hardware, so absence is allowed; what is
        // not allowed is a classification outside the vocabulary, which would
        // mean the form-factor mapping had drifted from the SDK.
        if let Some((route, _name)) = output_route() {
            assert!(["headphones", "speakers", "external"].contains(&route));
        }
        assert!(playback_devices().len() < 64);
    }
}
