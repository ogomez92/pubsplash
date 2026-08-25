//! The Core Audio primitives the rest of `audio` is built on: reading a
//! property off an audio object, listing devices, and turning the id kept in
//! the settings file into a device this boot.
//!
//! Like `convert.rs` and `render.rs`, this file is `#[path]`-included into the
//! standalone soundpack binaries and **must name nothing from this crate** — no
//! `crate::`, no `super::`. External crates are fine.
//!
//! ## The one thing to get right: a device id is a UID, never an `AudioDeviceID`
//!
//! `AudioDeviceID` is a handle the HAL hands out at runtime. It is not stable
//! across a reboot, a re-plug, or even a device being disabled and re-enabled,
//! and the HAL reuses the numbers. The settings file has to survive all of that,
//! so what is stored is `kAudioDevicePropertyDeviceUID` — a string the driver
//! owns, stable for the life of the hardware, and the same value another Mac's
//! copy of the settings file would fail to resolve, which is exactly right.
//!
//! [`device_for_uid`] is the one place that translation happens, and it answers
//! `None` for a UID no live device claims. That is the direct equivalent of the
//! `Active`-state check the Windows side documents at length: a configured
//! device that is not there must be an error the caller retries, never a silent
//! swap for the default one, because `capture` clears a Desktop Audio source
//! against whatever the output device is and a fallback could put the output
//! back on the endpoint that check just approved.
//!
//! ## Scopes
//!
//! Core Audio has no separate notion of a capture device and a render device —
//! there is one device list, and a device is an input or an output (or both,
//! which is what every USB interface is) according to whether it has streams in
//! that *scope*. So [`has_streams`] is what splits the list, and a device with
//! both appears in both, correctly.

#![allow(dead_code)]

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    AudioDeviceID, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectPropertyAddress, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyStreamConfiguration, kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject, kAudioObjectUnknown,
};
use objc2_core_audio_types::AudioBufferList;
use objc2_core_foundation::{CFRetained, CFString};

/// Which end of a device is being asked about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Input,
    Output,
}

impl Scope {
    fn raw(self) -> u32 {
        match self {
            Scope::Input => kAudioObjectPropertyScopeInput,
            Scope::Output => kAudioObjectPropertyScopeOutput,
        }
    }

    /// Which of the two "what is the default device" properties this scope
    /// means.
    fn default_selector(self) -> u32 {
        match self {
            Scope::Input => kAudioHardwarePropertyDefaultInputDevice,
            Scope::Output => kAudioHardwarePropertyDefaultOutputDevice,
        }
    }
}

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Reads a fixed-size property off an audio object.
///
/// `T` must be exactly what the HAL writes for this selector; a mismatch is a
/// size error from the API rather than a bad read, because the size is passed
/// in and checked.
fn property<T: Copy>(object: u32, selector: u32, scope: u32) -> Option<T> {
    let addr = address(selector, scope);
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut size = std::mem::size_of::<T>() as u32;
    // SAFETY: `addr` and `size` are live locals; `value` is sized for exactly
    // the `size` bytes the HAL is told it may write, and is only read below
    // when the call reported success.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(value.as_mut_ptr().cast::<c_void>())?,
        )
    };
    if status != 0 || size as usize != std::mem::size_of::<T>() {
        return None;
    }
    // SAFETY: the call succeeded and wrote exactly `size_of::<T>()` bytes.
    Some(unsafe { value.assume_init() })
}

/// Reads a variable-length property, asking the HAL for the size first.
///
/// The returned buffer is `u8` rather than the property's element type because
/// two of the callers want different types out of it and one of them
/// (`AudioBufferList`) is a header followed by a variable-length array that has
/// to be walked by hand.
fn property_bytes(object: u32, selector: u32, scope: u32) -> Option<Vec<u8>> {
    let addr = address(selector, scope);
    let mut size: u32 = 0;
    // SAFETY: both pointers are live locals for the duration of the call.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 {
        return None;
    }
    if size == 0 {
        return Some(Vec::new());
    }
    let mut buffer = vec![0u8; size as usize];
    // SAFETY: `buffer` is exactly `size` bytes, which is what the HAL was just
    // asked for and is what it is told it may write.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(buffer.as_mut_ptr().cast::<c_void>())?,
        )
    };
    if status != 0 {
        return None;
    }
    buffer.truncate(size as usize);
    Some(buffer)
}

/// Reads a `CFString` property and copies it out as a Rust `String`.
fn property_string(object: u32, selector: u32, scope: u32) -> Option<String> {
    // The HAL writes a `CFStringRef` the caller then owns, so this takes the
    // reference rather than borrowing it.
    let raw: *const CFString = property(object, selector, scope)?;
    let string = NonNull::new(raw.cast_mut())?;
    // SAFETY: the HAL returns a +1 reference for this family of properties, and
    // `from_raw` takes ownership of exactly that.
    let string = unsafe { CFRetained::from_raw(string) };
    Some(string.to_string())
}

/// Every audio device the system currently has, in the HAL's own order.
pub fn all_devices() -> Vec<AudioDeviceID> {
    let Some(bytes) = property_bytes(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
    ) else {
        return Vec::new();
    };
    bytes
        .chunks_exact(std::mem::size_of::<AudioDeviceID>())
        .map(|chunk| AudioDeviceID::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Whether `device` carries any stream in `scope` — which is what makes it an
/// input device or an output device. A device with both is both.
///
/// The property is an `AudioBufferList`: a `mNumberBuffers` count followed by
/// that many `AudioBuffer`s. Only the count is needed here, and it is read from
/// the header rather than by walking, so a truncated answer cannot mislead.
pub fn has_streams(device: AudioDeviceID, scope: Scope) -> bool {
    let Some(bytes) = property_bytes(
        device,
        kAudioDevicePropertyStreamConfiguration,
        scope.raw(),
    ) else {
        return false;
    };
    if bytes.len() < std::mem::size_of::<AudioBufferList>() {
        return false;
    }
    // SAFETY: the buffer is at least a whole `AudioBufferList` header, and only
    // the first field is read.
    let count = unsafe { (*bytes.as_ptr().cast::<AudioBufferList>()).mNumberBuffers };
    count > 0
}

/// The device's UID: the stable string the settings file stores. See the module
/// header for why this and not the `AudioDeviceID`.
pub fn device_uid(device: AudioDeviceID) -> Option<String> {
    property_string(
        device,
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
    )
}

/// The name a person would recognise the device by, as shown in Sound settings.
pub fn device_name(device: AudioDeviceID) -> Option<String> {
    property_string(
        device,
        kAudioObjectPropertyName,
        kAudioObjectPropertyScopeGlobal,
    )
}

/// The device a stored UID names, or `None` if nothing on this machine claims
/// it right now.
///
/// `kAudioHardwarePropertyTranslateUIDToDevice` is the HAL's own lookup and is
/// the right one to use: it is cheaper than walking every device, and it answers
/// `kAudioObjectUnknown` rather than an error for a UID that is simply not
/// present, which is the case that matters — an interface that has been
/// unplugged, or a settings file carried over from another Mac.
pub fn device_for_uid(uid: &str) -> Option<AudioDeviceID> {
    let cf = CFString::from_str(uid);
    let addr = address(
        kAudioHardwarePropertyTranslateUIDToDevice,
        kAudioObjectPropertyScopeGlobal,
    );
    let qualifier: *const CFString = &*cf;
    let mut device: AudioDeviceID = kAudioObjectUnknown;
    let mut size = std::mem::size_of::<AudioDeviceID>() as u32;
    // SAFETY: the qualifier is a live `CFString` for the duration of the call,
    // and `device` is exactly the size the HAL is told it may write.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object_system(),
            NonNull::from(&addr),
            std::mem::size_of::<*const CFString>() as u32,
            std::ptr::from_ref(&qualifier).cast::<c_void>(),
            NonNull::from(&mut size),
            NonNull::from(&mut device).cast::<c_void>(),
        )
    };
    if status != 0 || device == kAudioObjectUnknown {
        return None;
    }
    Some(device)
}

/// The system's current default device for `scope`, which is what a `None`
/// setting follows.
pub fn default_device(scope: Scope) -> Option<AudioDeviceID> {
    let device: AudioDeviceID = property(
        object_system(),
        scope.default_selector(),
        kAudioObjectPropertyScopeGlobal,
    )?;
    (device != kAudioObjectUnknown).then_some(device)
}

fn object_system() -> u32 {
    kAudioObjectSystemObject as u32
}

/// A device's UID and name together, which is the pair the pickers show.
///
/// A device with no UID is skipped rather than given a made-up one: the UID is
/// what gets written to the settings file, and an id that cannot be resolved
/// again is worse than an absent row.
pub fn describe(device: AudioDeviceID) -> Option<(String, String)> {
    let uid = device_uid(device)?;
    let name = device_name(device).unwrap_or_else(|| uid.clone());
    Some((uid, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These run against whatever hardware the machine has, so they assert
    /// invariants rather than specific devices — every Mac has at least one
    /// output, and every device the HAL lists has to round-trip through its own
    /// UID or the settings file cannot store it.
    #[test]
    fn every_listed_device_round_trips_through_its_uid() {
        for device in all_devices() {
            let Some(uid) = device_uid(device) else {
                continue;
            };
            assert_eq!(
                device_for_uid(&uid),
                Some(device),
                "device {device} did not resolve back from its own UID {uid:?}"
            );
        }
    }

    #[test]
    fn an_unknown_uid_resolves_to_nothing() {
        assert_eq!(device_for_uid("not-a-real-device-uid"), None);
        assert_eq!(device_for_uid(""), None);
    }

    /// The system default output has to be in the device list and has to have
    /// output streams, or the split in `has_streams` is wrong.
    #[test]
    fn the_default_output_is_an_output() {
        let Some(device) = default_device(Scope::Output) else {
            // A machine with no output at all is possible (a headless CI box).
            return;
        };
        assert!(all_devices().contains(&device));
        assert!(has_streams(device, Scope::Output));
    }

    /// Prints what the HAL reports on this machine. Not an assertion — a way to
    /// see the device list the pickers will show, the same way
    /// `vst::discover`'s `#[ignore]`d test shows the plugin scan.
    ///
    /// `cargo test coreaudio -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_the_devices_on_this_machine() {
        for device in all_devices() {
            let uid = device_uid(device).unwrap_or_else(|| "<no uid>".into());
            let name = device_name(device).unwrap_or_else(|| "<no name>".into());
            let mut ends = Vec::new();
            if has_streams(device, Scope::Input) {
                ends.push("input");
            }
            if has_streams(device, Scope::Output) {
                ends.push("output");
            }
            println!("{device:>4}  {:<14} {name}\n      {uid}", ends.join("+"));
        }
        println!(
            "default input:  {:?}\ndefault output: {:?}",
            default_device(Scope::Input).and_then(device_name),
            default_device(Scope::Output).and_then(device_name),
        );
    }
}
