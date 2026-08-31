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
//! ## AudioUnits
//!
//! [`open_output`] and [`open_input`] build the one kind of unit this app needs:
//! `kAudioUnitSubType_HALOutput`, bound to a specific device, with a client-side
//! format of 48 kHz interleaved stereo `f32` — the engine's own format, so the
//! HAL's converter does the sample-rate and layout work and nothing above here
//! has to.
//!
//! Two things about that unit are easy to get wrong and are done in the order
//! that works. **Enabling input and disabling output must happen before the
//! device is set**, because the HAL decides what it is wiring up at that moment.
//! And the client format goes on the *opposite* scope from the direction: an
//! output unit's client side is its **input** scope on element 0 (we feed it),
//! an input unit's client side is its **output** scope on element 1 (it feeds
//! us). Element 0 is always the hardware output bus and element 1 the hardware
//! input bus, whichever direction the unit runs in.
//!
//! ## Scopes
//!
//! Core Audio has no separate notion of a capture device and a render device —
//! there is one device list, and a device is an input or an output (or both,
//! which is what every USB interface is) according to whether it has streams in
//! that *scope*. So [`has_streams`] is what splits the list, and a device with
//! both appears in both, correctly.

// This file is primitives, and the subsystems that call them arrive one phase at
// a time -- `open_input`, `Unit::stop` and `Unit::raw` are what `capture` and
// `monitor` will open a device with, and are unused until then. Scoped to this
// file rather than the crate, deliberately: a blanket allow elsewhere would hide
// the real thing it is meant to catch.
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
use objc2_audio_toolbox::{
    AURenderCallback, AURenderCallbackStruct, AudioComponentFindNext, AudioComponentInstance,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioUnitInitialize,
    AudioUnitRenderActionFlags,
    AudioUnitRender, AudioUnitSetProperty, AudioUnitUninitialize, AudioComponentDescription,
    AudioOutputUnitStart,
    AudioOutputUnitStop, kAudioOutputUnitProperty_CurrentDevice,
    kAudioOutputUnitProperty_EnableIO, kAudioOutputUnitProperty_SetInputCallback,
    kAudioUnitManufacturer_Apple, kAudioUnitProperty_MaximumFramesPerSlice,
    kAudioUnitProperty_SetRenderCallback,
    kAudioUnitProperty_StreamFormat, kAudioUnitScope_Global, kAudioUnitScope_Input,
    kAudioUnitScope_Output, kAudioUnitSubType_HALOutput, kAudioUnitType_Output,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
};
use objc2_core_foundation::{CFRetained, CFString};

/// Must match `convert::ENGINE_SAMPLE_RATE`. Spelled out rather than imported
/// because this file is compiled in two positions — as `audio::coreaudio` and
/// as `audio::render`'s `ca` — and no one `super::` path names the constants
/// from both.
pub const SAMPLE_RATE: u32 = 48_000;
/// Must match `convert::ENGINE_CHANNELS`, and `render.rs`'s copy of it.
pub const CHANNELS: usize = 2;

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

// --- AudioUnits -------------------------------------------------------------

/// Renders an `OSStatus` the way Core Audio's own documentation writes them.
///
/// Most of these codes are four-character constants — `fmt?` for an
/// unsupported format, `!obj` for a stale object — and the number alone is
/// unreadable. Anything that is not four printable characters is a plain
/// integer, which is what the negative errors are.
pub fn status_message(status: i32) -> String {
    let bytes = (status as u32).to_be_bytes();
    if bytes.iter().all(|b| (0x20..0x7f).contains(b)) {
        format!("{} ({status})", String::from_utf8_lossy(&bytes))
    } else {
        status.to_string()
    }
}

/// How many channels `device` carries in `scope`.
///
/// **The capture path has to ask, because AUHAL will not change a channel
/// count.** It resamples happily — a 24 kHz microphone delivers 48 kHz with
/// nothing asked of us — but ask it for two channels from a one-channel device
/// and the format is *accepted*, the unit initializes, starts, and reports
/// itself running, while every single `AudioUnitRender` answers
/// `kAudioUnitErr_CannotDoInCurrentContext` (-10863). Nothing else says
/// anything is wrong. That is the same failure [`crate::audio::tap`]'s
/// `set_engine_rate` exists for, and it is why the count is negotiated rather
/// than assumed: a built-in Mac microphone is mono, as is every headset, so
/// assuming stereo means no microphone works at all.
///
/// The property is a variable-length `AudioBufferList`: a count followed by that
/// many `AudioBuffer`s. The channels are **summed** across them rather than read
/// from the first, because a device may present its channels as several
/// one-channel streams instead of one many-channel stream.
pub fn channels_in(device: AudioDeviceID, scope: Scope) -> Option<usize> {
    let bytes = property_bytes(
        device,
        kAudioDevicePropertyStreamConfiguration,
        scope.raw(),
    )?;
    // The declared size of `AudioBufferList` includes its first `AudioBuffer`;
    // the header alone is what precedes the array.
    let header = std::mem::size_of::<AudioBufferList>() - std::mem::size_of::<AudioBuffer>();
    if bytes.len() < header {
        return None;
    }
    // SAFETY: the buffer is at least a whole header, and only the count is read.
    let count = unsafe { (*bytes.as_ptr().cast::<AudioBufferList>()).mNumberBuffers } as usize;
    let mut total = 0usize;
    for index in 0..count {
        let offset = header + index * std::mem::size_of::<AudioBuffer>();
        if offset + std::mem::size_of::<AudioBuffer>() > bytes.len() {
            // A truncated answer is a short read, never a walk off the end.
            break;
        }
        // SAFETY: the bounds check above proves this `AudioBuffer` is wholly
        // inside `bytes`; it is read by value rather than borrowed.
        let buffer = unsafe { bytes.as_ptr().add(offset).cast::<AudioBuffer>().read_unaligned() };
        total += buffer.mNumberChannels as usize;
    }
    (total > 0).then_some(total)
}

/// The device's own sample rate.
pub fn nominal_rate(device: AudioDeviceID) -> Option<f64> {
    let bytes = property_bytes(
        device,
        objc2_core_audio::kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
    )?;
    (bytes.len() >= std::mem::size_of::<f64>())
        .then(|| unsafe { bytes.as_ptr().cast::<f64>().read_unaligned() })
}

/// The most channels a capture device may present before the extra ones are
/// ignored.
///
/// Not a hardware limit — a guard on the one allocation [`Scratch`] makes from a
/// number the driver chose. Only the first two are ever used (see
/// [`Scratch::render`]), so the cap costs nothing a caller can hear.
pub const MAX_INPUT_CHANNELS: usize = 64;

/// The format the *app* takes from a capture device: [`SAMPLE_RATE`], 32-bit
/// float, interleaved, at whatever channel count the device actually has.
///
/// The rate is ours because AUHAL converts it; the channel count is the
/// device's because AUHAL does not. See [`channels_in`].
fn input_format(channels: usize) -> AudioStreamBasicDescription {
    let bytes_per_frame = (channels * std::mem::size_of::<f32>()) as u32;
    AudioStreamBasicDescription {
        mSampleRate: f64::from(SAMPLE_RATE),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: bytes_per_frame,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes_per_frame,
        mChannelsPerFrame: channels as u32,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// The engine's format, as Core Audio describes it: 48 kHz, two channels,
/// 32-bit float, **interleaved**.
///
/// Interleaved because that is what the mixer produces and consumes; the
/// alternative, Core Audio's non-interleaved "canonical" layout, would mean a
/// de-interleave on one side and a re-interleave on the other for no gain.
/// Interleaved is the absence of `kAudioFormatFlagIsNonInterleaved`, so one
/// buffer carries both channels and a frame is eight bytes.
fn engine_format() -> AudioStreamBasicDescription {
    let bytes_per_frame = (CHANNELS * std::mem::size_of::<f32>()) as u32;
    AudioStreamBasicDescription {
        mSampleRate: f64::from(SAMPLE_RATE),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: bytes_per_frame,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes_per_frame,
        mChannelsPerFrame: CHANNELS as u32,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// A live `AudioUnit`, stopped and disposed when dropped.
///
/// **The drop order is the whole safety argument.** `AudioOutputUnitStop`
/// returns only once the render callback has finished and will not be entered
/// again, so everything the callback borrows — the ring buffer, the sample
/// slice, the stop flag — is safe to drop after this is. That is what lets the
/// callers pass a pointer to a stack local as the callback's context: the unit
/// is created after it and dropped before it.
pub struct Unit(AudioComponentInstance);

// SAFETY: an `AudioComponentInstance` is an opaque handle with no thread
// affinity; the HAL serializes its own access. The callback runs on a real-time
// thread the HAL owns either way.
unsafe impl Send for Unit {}

impl Unit {
    /// Starts the unit. The callback begins being called on the HAL's own
    /// real-time thread from here.
    pub fn start(&self) -> Result<(), String> {
        // SAFETY: `self.0` is a live instance for the life of `self`.
        let status = unsafe { AudioOutputUnitStart(self.0) };
        if status == 0 {
            Ok(())
        } else {
            Err(format!("starting the audio unit: {}", status_message(status)))
        }
    }

    /// Stops the unit early. Idempotent, and also done by `Drop`.
    pub fn stop(&self) {
        // SAFETY: as `start`.
        unsafe {
            AudioOutputUnitStop(self.0);
        }
    }

    /// The raw instance, for the one caller that has to call `AudioUnitRender`
    /// on it from inside its own callback.
    pub fn raw(&self) -> AudioComponentInstance {
        self.0
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        // SAFETY: `self.0` is live and this runs once. Stop first, so the
        // callback is not running while the unit is torn down under it.
        unsafe {
            AudioOutputUnitStop(self.0);
            AudioUnitUninitialize(self.0);
            AudioComponentInstanceDispose(self.0);
        }
    }
}

fn set_property<T>(
    unit: AudioComponentInstance,
    id: u32,
    scope: u32,
    element: u32,
    value: &T,
    what: &str,
) -> Result<(), String> {
    // SAFETY: `value` is a live `T` and its size is passed alongside it.
    let status = unsafe {
        AudioUnitSetProperty(
            unit,
            id,
            scope,
            element,
            std::ptr::from_ref(value).cast::<c_void>(),
            std::mem::size_of::<T>() as u32,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{what}: {}", status_message(status)))
    }
}

/// The hardware output bus. Element 0 on a HAL unit, always.
const BUS_OUTPUT: u32 = 0;
/// The hardware input bus. Element 1 on a HAL unit, always.
const BUS_INPUT: u32 = 1;

/// Creates an uninitialized HAL unit. Shared by both directions.
fn new_hal_unit() -> Result<AudioComponentInstance, String> {
    let description = AudioComponentDescription {
        componentType: kAudioUnitType_Output,
        componentSubType: kAudioUnitSubType_HALOutput,
        componentManufacturer: kAudioUnitManufacturer_Apple,
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    // SAFETY: `description` is a live local; a null first argument starts the
    // search from the beginning of the component list.
    let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), NonNull::from(&description)) };
    if component.is_null() {
        return Err("this system has no HAL output audio unit".to_string());
    }
    let mut instance: AudioComponentInstance = std::ptr::null_mut();
    // SAFETY: `instance` is a live local the call writes on success.
    let status = unsafe { AudioComponentInstanceNew(component, NonNull::from(&mut instance)) };
    if status != 0 || instance.is_null() {
        return Err(format!(
            "creating the audio unit: {}",
            status_message(status)
        ));
    }
    Ok(instance)
}

/// Opens an output unit on `device` that pulls from `callback`.
///
/// `ref_con` is handed back to the callback untouched. It must stay valid until
/// the returned [`Unit`] is dropped — see that type for why the drop order
/// makes that a real guarantee rather than a hope.
///
/// # Safety
/// `callback` must be a valid `AURenderCallback` and `ref_con` must be valid
/// for as long as the returned `Unit` lives.
pub unsafe fn open_output(
    device: AudioDeviceID,
    callback: AURenderCallback,
    ref_con: *mut c_void,
) -> Result<Unit, String> {
    let unit = new_hal_unit()?;
    // Bound to a `Unit` immediately, so every `?` below disposes it rather than
    // leaking a component instance.
    let unit = Unit(unit);

    set_property(
        unit.0,
        kAudioOutputUnitProperty_CurrentDevice,
        kAudioUnitScope_Global,
        BUS_OUTPUT,
        &device,
        "choosing the playback device",
    )?;
    // The client side of an output unit is its *input* scope: we feed it.
    set_property(
        unit.0,
        kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Input,
        BUS_OUTPUT,
        &engine_format(),
        "setting the playback format",
    )?;
    set_property(
        unit.0,
        kAudioUnitProperty_SetRenderCallback,
        kAudioUnitScope_Input,
        BUS_OUTPUT,
        &AURenderCallbackStruct {
            inputProc: callback,
            inputProcRefCon: ref_con,
        },
        "installing the playback callback",
    )?;
    initialize(&unit)?;
    Ok(unit)
}

/// Opens an input unit on `device` that calls `callback` when audio has
/// arrived. The callback is expected to call `AudioUnitRender` to collect it.
///
/// # Safety
/// As [`open_output`].
pub unsafe fn open_input(
    device: AudioDeviceID,
    channels: usize,
    callback: AURenderCallback,
    ref_con: *mut c_void,
) -> Result<Unit, String> {

    let unit = Unit(new_hal_unit()?);

    // Before the device, and in this order. A HAL unit is an *output* unit by
    // default; the HAL wires up what it is going to do at the moment the device
    // is set, so flipping these afterwards is silently ineffective.
    let enable: u32 = 1;
    let disable: u32 = 0;
    set_property(
        unit.0,
        kAudioOutputUnitProperty_EnableIO,
        kAudioUnitScope_Input,
        BUS_INPUT,
        &enable,
        "enabling audio input",
    )?;
    set_property(
        unit.0,
        kAudioOutputUnitProperty_EnableIO,
        kAudioUnitScope_Output,
        BUS_OUTPUT,
        &disable,
        "disabling output on the capture unit",
    )?;
    set_property(
        unit.0,
        kAudioOutputUnitProperty_CurrentDevice,
        kAudioUnitScope_Global,
        BUS_OUTPUT,
        &device,
        "choosing the capture device",
    )?;
    // The client side of an input unit is its *output* scope: it feeds us.
    //
    // `channels` must be the device's own count and must match the [`Scratch`]
    // the callback renders into — see [`channels_in`] for what asking for the
    // wrong one does, which is nothing visible until no audio ever arrives.
    set_property(
        unit.0,
        kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Output,
        BUS_INPUT,
        &input_format(channels),
        "setting the capture format",
    )?;
    // Pinned rather than left at whatever the device wants, because the callback
    // may not allocate: [`Scratch`] has to be sized before the unit starts, and
    // this is the number that bounds it. Set before `initialize`, which is when
    // the unit sizes its own buffers.
    set_property(
        unit.0,
        kAudioUnitProperty_MaximumFramesPerSlice,
        kAudioUnitScope_Global,
        BUS_OUTPUT,
        &MAX_FRAMES_PER_SLICE,
        "limiting the capture block size",
    )?;
    set_property(
        unit.0,
        kAudioOutputUnitProperty_SetInputCallback,
        kAudioUnitScope_Global,
        BUS_OUTPUT,
        &AURenderCallbackStruct {
            inputProc: callback,
            inputProcRefCon: ref_con,
        },
        "installing the capture callback",
    )?;
    initialize(&unit)?;
    Ok(unit)
}

/// The interleaved `f32` slice one HAL cycle's buffer list carries, or `None`
/// if the list is not the single interleaved buffer both directions ask for.
///
/// Shared by every callback in the app — [`crate::audio::render`]'s cue
/// playback, `monitor`'s tap and `capture`'s microphone — because the checks
/// are the same in both directions and each of them is a bug that only shows up
/// on somebody else's hardware.
///
/// The returned slice is bounded by the *smaller* of what the list says it
/// holds and what the HAL asked for this cycle, so a disagreement between the
/// HAL's two accounts of its own buffer is a short read or write rather than an
/// overrun. Callers must therefore work from `slice.len()` and never from
/// `frames`.
///
/// # Safety
/// `io_data` must be null or a valid `AudioBufferList` from the HAL, and the
/// returned slice borrows it for the rest of the cycle.
pub unsafe fn hal_buffer<'a>(io_data: *mut AudioBufferList, frames: u32) -> Option<&'a mut [f32]> {
    if io_data.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees the pointer.
    let list = unsafe { &mut *io_data };
    if list.mNumberBuffers < 1 {
        return None;
    }
    // SAFETY: `mBuffers` is a C flexible array member with at least
    // `mNumberBuffers` entries, checked to be non-zero above.
    let buffer = unsafe { &mut *list.mBuffers.as_mut_ptr() };
    // One buffer carrying both channels is what an interleaved format means. A
    // pair of mono buffers would be the non-interleaved layout, and treating
    // the first of them as interleaved stereo is a mangled left channel;
    // silence is the better wrong answer.
    if buffer.mNumberChannels as usize != CHANNELS {
        return None;
    }
    let data = buffer.mData.cast::<f32>();
    if data.is_null() {
        return None;
    }
    let capacity = buffer.mDataByteSize as usize / std::mem::size_of::<f32>();
    let wanted = frames as usize * CHANNELS;
    // SAFETY: `data` points at `capacity` floats by the HAL's own account.
    Some(unsafe { std::slice::from_raw_parts_mut(data, capacity.min(wanted)) })
}

/// The most frames the HAL may hand a callback in one cycle.
///
/// Chosen rather than queried, and then imposed on the unit, because the
/// alternative is a callback that has to allocate when a cycle comes up bigger
/// than expected — which is the one thing a real-time thread may not do. 4096 is
/// far above any real device period (a 48 kHz device runs 512 frames or fewer by
/// default) and costs 32 KB of scratch per source.
pub const MAX_FRAMES_PER_SLICE: u32 = 4096;

/// A buffer list the *app* owns.
///
/// Playback and capture are not symmetrical here: on the way out the HAL hands
/// the callback its own buffer to fill ([`hal_buffer`]), but on the way in the
/// callback is only a notification — the audio has to be fetched with
/// [`Scratch::render`], into memory the app supplies. This is that memory,
/// allocated once when the source opens and reused every cycle.
pub struct Scratch {
    /// What the device hands over, interleaved at [`Self::channels`].
    samples: Vec<f32>,
    /// The stereo the rest of the app works in, when the device is not already
    /// stereo. Allocated with everything else so [`Self::render`] can convert
    /// without allocating on the HAL's thread.
    stereo: Vec<f32>,
    /// The device's channel count, which must be the one `open_input` asked for.
    channels: usize,
    /// [`SAMPLE_RATE`] over the device's own rate — how many client frames one
    /// device frame becomes. See [`Self::client_frames`].
    ratio: f64,
    /// The fraction of a frame left over from the last cycle, carried so the
    /// long-run rate is exact rather than drifting by up to a frame a cycle.
    /// Only ever non-zero when [`Self::ratio`] is not a whole number.
    carry: f64,
    list: AudioBufferList,
}

impl Scratch {
    /// Allocates for [`MAX_FRAMES_PER_SLICE`] frames of a `channels`-channel
    /// device, which is what `open_input` holds the unit to.
    ///
    /// **Created before the unit, always.** The callback reaches this through a
    /// raw pointer, and `Unit::drop` is what guarantees the callback has
    /// finished — so the unit must be the shorter-lived of the two.
    pub fn for_device(channels: usize, device_rate: f64) -> Self {
        let channels = channels.clamp(1, MAX_INPUT_CHANNELS);
        // A device that will not say what rate it runs at is taken to be at
        // ours, which is the no-conversion case and what every aggregate this
        // app builds for itself actually is.
        let ratio = if device_rate > 0.0 {
            f64::from(SAMPLE_RATE) / device_rate
        } else {
            1.0
        };
        // Sized for the *client* frames a full device slice becomes, which is
        // the larger of the two whenever the device runs slower than we do.
        let max_frames = (MAX_FRAMES_PER_SLICE as f64 * ratio).ceil() as usize + 1;
        Self {
            samples: vec![0.0; max_frames * channels],
            // Only needed when a conversion actually happens, but sized
            // unconditionally: a zero-length `Vec` allocates nothing, and
            // branching on it here would put the decision in the callback.
            stereo: vec![
                0.0;
                if channels == CHANNELS {
                    0
                } else {
                    max_frames * CHANNELS
                }
            ],
            channels,
            ratio,
            carry: 0.0,
            list: AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: channels as u32,
                    mDataByteSize: 0,
                    mData: std::ptr::null_mut(),
                }],
            },
        }
    }

    /// Fetches one cycle's audio from `unit` and returns it, interleaved.
    ///
    /// The returned slice is bounded by what the unit says it actually wrote,
    /// not by `frames`, for [`hal_buffer`]'s reason: the HAL's two accounts of a
    /// buffer are allowed to disagree and a short read is the safe reading.
    ///
    /// A cycle asking for more than [`MAX_FRAMES_PER_SLICE`] is refused rather
    /// than truncated. Truncating would silently drop audio for as long as the
    /// device kept it up; failing gets the source reopened and a line in the log.
    ///
    /// # Safety
    /// `unit` must be a live input unit, and `flags` and `time` the ones the HAL
    /// passed this cycle.
    pub unsafe fn render(
        &mut self,
        unit: AudioComponentInstance,
        flags: NonNull<AudioUnitRenderActionFlags>,
        time: NonNull<AudioTimeStamp>,
        frames: u32,
    ) -> Result<&[f32], i32> {
        let frames = self.client_frames(frames);
        let wanted = frames as usize * self.channels;
        if wanted > self.samples.len() {
            return Err(-1);
        }
        self.list.mBuffers[0].mData = self.samples.as_mut_ptr().cast::<c_void>();
        self.list.mBuffers[0].mDataByteSize = (wanted * std::mem::size_of::<f32>()) as u32;
        // SAFETY: the list points at `self.samples`, which is at least `wanted`
        // floats long by the check above, and lives as long as `self`.
        let status = unsafe {
            AudioUnitRender(
                unit,
                flags.as_ptr(),
                time,
                BUS_INPUT,
                frames,
                NonNull::from(&mut self.list),
            )
        };
        if status != 0 {
            return Err(status);
        }
        let written = self.list.mBuffers[0].mDataByteSize as usize / std::mem::size_of::<f32>();
        Ok(self.as_stereo(written.min(wanted)))
    }

    /// How many frames to ask [`AudioUnitRender`] for, given the `device_frames`
    /// the HAL says this cycle carries.
    ///
    /// **AUHAL resamples, but the count must be expressed at *our* rate, not the
    /// device's** — and that was measured rather than assumed. Against a 24 kHz
    /// microphone with a 48 kHz client format, asking for the HAL's own frame
    /// count answers `kAudioUnitErr_CannotDoInCurrentContext` (-10863) on every
    /// single cycle, while the unit initializes, starts and reports itself
    /// running; asking for twice it delivers cleanly. Asking for *half* fails
    /// too, so this is not "at most this many" — the converter wants exactly the
    /// frames this device slice becomes.
    ///
    /// The ratio is rarely a whole number (44.1 kHz into 48 kHz is 1.088...), so
    /// the fraction is carried between cycles instead of being rounded away. A
    /// rounded count would be wrong by up to a frame each cycle, always in the
    /// same direction — about two milliseconds a second at a typical block size,
    /// which is a drift the ring buffer eventually pays for in dropped samples.
    fn client_frames(&mut self, device_frames: u32) -> u32 {
        if self.ratio == 1.0 {
            return device_frames;
        }
        let exact = f64::from(device_frames) * self.ratio + self.carry;
        let whole = exact.floor();
        self.carry = exact - whole;
        whole as u32
    }

    /// The device's own frame counter, restated at our rate.
    ///
    /// `AudioTimeStamp::mSampleTime` counts *device* frames, while everything
    /// this hands back is counted in client frames — so the two must be put on
    /// one scale before [`crate::audio::health::DeviceTimeline`] differences
    /// them. Feeding it the raw counter against a converted sample count makes
    /// every cycle look like a gap and the measured rate collapse to nothing,
    /// which turns the health line from instrumentation into noise.
    pub fn client_index(&self, device_index: f64) -> u64 {
        (device_index * self.ratio) as u64
    }

    /// Converts the `len` samples just rendered into the interleaved stereo the
    /// mixer takes.
    ///
    /// **A mono device is the common case, not the exotic one**: every built-in
    /// Mac microphone and every headset is one channel, so this runs for most
    /// users. Mono is copied to both ears rather than to the left one, which is
    /// the difference between a microphone that sounds centred and one that
    /// sounds broken.
    ///
    /// Anything wider than stereo is taken as its **first two channels** — the
    /// first stereo pair, which is what an interface's inputs 1 and 2 are. A
    /// downmix of every channel would fold unrelated inputs together, which is
    /// worse and less predictable than ignoring them.
    ///
    /// No allocation: both buffers were sized in [`Self::for_channels`].
    fn as_stereo(&mut self, len: usize) -> &[f32] {
        if self.channels == CHANNELS {
            return &self.samples[..len];
        }
        let frames = len / self.channels;
        for frame in 0..frames {
            let from = frame * self.channels;
            let to = frame * CHANNELS;
            let (left, right) = if self.channels == 1 {
                (self.samples[from], self.samples[from])
            } else {
                (self.samples[from], self.samples[from + 1])
            };
            self.stereo[to] = left;
            self.stereo[to + 1] = right;
        }
        &self.stereo[..frames * CHANNELS]
    }
}

impl Default for Scratch {
    fn default() -> Self {
        Self::for_device(CHANNELS, f64::from(SAMPLE_RATE))
    }
}

fn initialize(unit: &Unit) -> Result<(), String> {
    // SAFETY: `unit.0` is a live, fully configured instance.
    let status = unsafe { AudioUnitInitialize(unit.0) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "the device would not accept {SAMPLE_RATE} Hz 32-bit float: {}",
            status_message(status)
        ))
    }
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

    /// The rate a device runs at is rarely a whole multiple of ours, and the
    /// leftover fraction is what keeps the long-run count exact. 44.1 kHz into
    /// 48 kHz is the case this machine cannot produce and every USB interface
    /// can.
    #[test]
    fn a_fractional_rate_keeps_its_leftovers() {
        let mut scratch = Scratch::for_device(1, 44_100.0);
        let cycles = 1_000;
        let device_frames = 512u32;
        let total: u64 = (0..cycles)
            .map(|_| u64::from(scratch.client_frames(device_frames)))
            .sum();
        // What 1000 blocks of 512 frames at 44.1 kHz is worth at 48 kHz.
        let exact = f64::from(device_frames) * f64::from(SAMPLE_RATE) / 44_100.0 * f64::from(cycles);
        let drift = (total as f64 - exact).abs();
        assert!(
            drift < 1.0,
            "asked for {total} frames where {exact} were due: {drift} adrift"
        );
    }

    /// The whole-number case must not be pushed through the arithmetic at all,
    /// since a device at our own rate is the common one.
    #[test]
    fn a_matching_rate_asks_for_exactly_what_it_is_given() {
        let mut scratch = Scratch::for_device(CHANNELS, f64::from(SAMPLE_RATE));
        assert_eq!(scratch.client_frames(512), 512);
        assert_eq!(scratch.client_frames(37), 37);
    }

    /// Halving the device rate doubles the frames, which is the case measured
    /// against a real Bluetooth headset: 24 kHz in, 48 kHz out.
    #[test]
    fn a_half_rate_device_asks_for_twice_as_many() {
        let mut scratch = Scratch::for_device(1, 24_000.0);
        assert_eq!(scratch.client_frames(512), 1024);
    }

    /// A mono microphone -- every built-in Mac one, and every headset -- must
    /// reach both ears, not just the left.
    #[test]
    fn mono_is_copied_to_both_ears() {
        let mut scratch = Scratch::for_device(1, f64::from(SAMPLE_RATE));
        scratch.samples[..3].copy_from_slice(&[0.25, -0.5, 1.0]);
        assert_eq!(scratch.as_stereo(3), &[0.25, 0.25, -0.5, -0.5, 1.0, 1.0]);
    }

    /// Stereo is handed straight back: no copy, no conversion.
    #[test]
    fn stereo_is_passed_through_untouched() {
        let mut scratch = Scratch::for_device(CHANNELS, f64::from(SAMPLE_RATE));
        scratch.samples[..4].copy_from_slice(&[0.1, 0.2, 0.3, 0.4]);
        assert_eq!(scratch.as_stereo(4), &[0.1, 0.2, 0.3, 0.4]);
    }

    /// Anything wider than stereo is its first pair -- an interface's inputs 1
    /// and 2 -- rather than a fold-down of channels that have nothing to do with
    /// each other.
    #[test]
    fn a_wide_device_is_taken_as_its_first_pair() {
        let mut scratch = Scratch::for_device(4, f64::from(SAMPLE_RATE));
        scratch.samples[..8].copy_from_slice(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        assert_eq!(scratch.as_stereo(8), &[1.0, 2.0, 5.0, 6.0]);
    }

    /// The buffer has to be big enough for the frames a slow device's slice
    /// *becomes*, not the ones it arrives as, or every cycle is refused.
    #[test]
    fn the_buffer_is_sized_for_the_converted_frames() {
        let scratch = Scratch::for_device(1, 24_000.0);
        assert!(
            scratch.samples.len() >= MAX_FRAMES_PER_SLICE as usize * 2,
            "a full slice from a half-rate device must fit"
        );
    }
}
