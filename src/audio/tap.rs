//! Core Audio **process taps**: capturing what other applications are playing.
//!
//! This is macOS's answer to the two WASAPI loopback forms `audio::capture`
//! uses on Windows, and the mapping is closer than it has any right to be:
//!
//! - **Desktop Audio** defaults to "everything except Pubsplash's own output",
//!   which is `CATapDescription`'s `initStereoGlobalTapButExcludeProcesses`
//!   almost word for word — the same rule the Windows side gets from
//!   `new_application_loopback_client(our_pid, false)`.
//! - An **Application** source is `initStereoMixdownOfProcesses` over the
//!   processes that make up that app.
//!
//! Requires macOS 14.4, and the **audio-capture consent**: the first tap prompts
//! the user, and a refusal makes the create fail. That is reported as an
//! ordinary capture failure, which the supervisor in `audio::capture` already
//! knows how to show and retry — the source appears in the mixer saying why it
//! is not running rather than pretending to be live.
//!
//! ## The shape, and why there are two objects rather than one
//!
//! A tap is not a device and cannot be read from. It is a *source* that has to
//! be put inside an **aggregate device**, and the aggregate device is what an
//! `AudioUnit` opens. So [`ProcessTap`] owns both and destroys both, in that
//! order, and hands out an ordinary `AudioDeviceID` that
//! [`crate::audio::coreaudio::open_input`] opens like any microphone. Everything
//! downstream — the callback, the ring, the health accounting — is the
//! microphone path unchanged.
//!
//! The aggregate is created **private** (`kAudioAggregateDeviceIsPrivateKey`),
//! which is load-bearing rather than tidy: a non-private aggregate appears in
//! every other app's device list and in System Settings for as long as it
//! exists, so a crash would leave litter in the user's sound settings that only
//! a reboot clears.
//!
//! ## The process-tree rule, rewritten rather than ported
//!
//! Windows opens an Application source with
//! `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`, so naming the root of
//! the tree covers every child — which matters because Chromium, Electron and
//! Spotify all play their audio from a `--type=utility` child rather than from
//! the process that owns the window.
//!
//! **A tap has no such flag.** It takes a list of process objects and taps
//! exactly those. So the walk that `device::choose_pid` does to find the root of
//! the tree is only half the job here: [`tree_of`] then walks back *down* it and
//! every descendant is named explicitly. Getting this wrong is the same silent
//! failure as on Windows — a tap that opens happily and delivers nothing.
// The global-tap half of this file is written, measured and switched off; see
// `capture::imp::DESKTOP_AUDIO_UNSAFE` for what was measured and why. It stays
// compiled -- and its tests keep running -- so whoever finishes it starts from
// working code rather than from this header. Scoped to this file with a note,
// not a crate-wide allow.
#![allow(dead_code)]

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_audio::{
    AudioDeviceID, AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap, AudioObjectID,
    AudioObjectGetPropertyData, AudioObjectPropertyAddress, AudioObjectSetPropertyData,
    CATapDescription, kAudioDevicePropertyNominalSampleRate,
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioObjectUnknown, kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey,
};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};
use objc2_audio_toolbox::AudioUnitRenderActionFlags;
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};
use std::ffi::CStr;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::coreaudio::status_message;
use crate::audio::mixer::{CHANNELS, SAMPLE_RATE};

/// The `AudioObjectID` Core Audio uses for a running process, or `None` if it
/// has none — which is the ordinary answer for a process that has never played
/// audio.
pub fn process_object_for_pid(pid: u32) -> Option<AudioObjectID> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyTranslatePIDToProcessObject,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let pid = pid as i32;
    let mut object: AudioObjectID = kAudioObjectUnknown;
    let mut size = std::mem::size_of::<AudioObjectID>() as u32;
    // SAFETY: the qualifier is one `pid_t` and its size is passed alongside it;
    // `object` and `size` are live locals the call writes on success.
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            std::ptr::NonNull::from(&address),
            std::mem::size_of::<i32>() as u32,
            std::ptr::from_ref(&pid).cast(),
            std::ptr::NonNull::from(&mut size),
            std::ptr::NonNull::from(&mut object).cast(),
        )
    };
    (status == 0 && object != kAudioObjectUnknown).then_some(object)
}


/// Every pid in the process tree rooted at `root`, `root` itself included.
///
/// See the module header: a tap has no "and its descendants" flag, so the tree
/// that Windows gets from one flag has to be spelled out. Built from the same
/// `sysinfo` snapshot `audio::device` already uses, so no second process
/// enumeration is introduced.
///
/// Depth is bounded by the number of processes rather than by recursion: a
/// parent cycle would otherwise hang the capture thread, and while the kernel
/// does not produce one, this reads a snapshot that can be inconsistent.
pub fn tree_of(root: u32, processes: &[(u32, Option<u32>)]) -> Vec<u32> {
    let mut tree = vec![root];
    let mut added = true;
    while added {
        added = false;
        for (pid, parent) in processes {
            if tree.contains(pid) {
                continue;
            }
            if parent.is_some_and(|parent| tree.contains(&parent)) {
                tree.push(*pid);
                added = true;
            }
        }
        if tree.len() > processes.len() {
            break;
        }
    }
    tree
}

/// Puts the aggregate device on the engine's own sample rate.
///
/// **Without this the tap does not deliver a single frame**, and the failure is
/// not obvious: an aggregate built around a tap comes up at whatever rate the
/// tapped streams happen to be at — 24 kHz on an idle Mac — and an input
/// `AudioUnit` asked for 48 kHz on top of that answers every render with
/// `kAudioUnitErr_CannotDoInCurrentContext` rather than resampling. The unit
/// opens, starts, and reports itself running; only the frame count gives it
/// away.
///
/// Setting the rate is safe precisely because the aggregate is **private and
/// ours**: it is created per source, nothing else can be listening to it, and it
/// is destroyed with the tap. Doing the same to a real device would be changing
/// a setting out from under the rest of the system.
///
/// A failure here is not fatal — the device may simply already be at the right
/// rate — so it is logged rather than returned, and the open that follows is
/// what really decides.
fn set_engine_rate(device: AudioDeviceID) {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyNominalSampleRate,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let rate = f64::from(super::coreaudio::SAMPLE_RATE);
    // SAFETY: `rate` is one live `f64` and its size is passed alongside it.
    let status = unsafe {
        AudioObjectSetPropertyData(
            device,
            std::ptr::NonNull::from(&address),
            0,
            std::ptr::null(),
            std::mem::size_of::<f64>() as u32,
            std::ptr::NonNull::from(&rate).cast(),
        )
    };
    if status != 0 {
        log::debug!(
            "Could not put the capture device on {rate} Hz: {}",
            status_message(status)
        );
    }
}

/// A silent output stream held open for as long as any tap exists.
///
/// **This is what makes the exclusion actually exclude us**, and it is not
/// optional. `CATapDescription`'s exclusion binds to processes that have a
/// *live audio stream at the moment the tap is created*: a process that merely
/// owns an audio object, with nothing playing, is not excluded, and everything
/// it plays afterwards lands in the tap. Measured, not guessed — a tap created
/// while Pubsplash was silent captured its own tone at 0.30, and the same tap
/// created a fraction of a second later, with the tone already running, captured
/// 0.0000.
///
/// That matters because Pubsplash's output is *intermittent*. A cue opens a unit
/// for the length of one sound; the monitor thread only runs while a strip is
/// monitored. So at the moment a Desktop Audio source opens, there is very often
/// nothing playing at all — and without this the source would capture every
/// spoken chat message and every cue for the rest of the session, which is the
/// exact failure the Windows side's `would_capture_pubsplash` exists to prevent.
///
/// The cost is one open output stream rendering silence while a Desktop Audio
/// source exists. It is refcounted rather than per-tap so two such sources cost
/// one stream, and it is released with the last tap, so a user with no Desktop
/// Audio source never pays for it.
struct Presence {
    /// Dropped first, so the callback has stopped before `cycles` goes away.
    _unit: super::coreaudio::Unit,
    /// Boxed for a stable address: the callback holds a pointer to it.
    _cycles: Box<AtomicU64>,
}

/// The live presence stream and how many taps are relying on it.
static PRESENCE: Mutex<(Option<Presence>, usize)> = Mutex::new((None, 0));

/// The render callback for [`Presence`]: silence, and a count of its own cycles.
///
/// The count is what [`acquire_presence`] waits on. `AudioOutputUnitStart`
/// returns as soon as the HAL has accepted the unit, not once it is rendering,
/// and creating the tap in that gap is the same race as having no presence
/// stream at all — measured as one leak in six runs before this was added.
///
/// # Safety
/// `ref_con` is the `AtomicU64` handed to `open_output`, alive for the life of
/// the unit; `io_data` is the HAL's buffer list for this cycle.
unsafe extern "C-unwind" fn render_silence(
    ref_con: NonNull<std::ffi::c_void>,
    _flags: NonNull<AudioUnitRenderActionFlags>,
    _time: NonNull<AudioTimeStamp>,
    _bus: u32,
    frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    // SAFETY: the counter outlives the unit, by the contract above.
    let cycles = unsafe { &*ref_con.as_ptr().cast::<AtomicU64>() };
    cycles.fetch_add(1, Ordering::Relaxed);
    // SAFETY: the HAL's own buffer for this cycle.
    if let Some(out) = unsafe { super::coreaudio::hal_buffer(io_data, frames) } {
        if PROBING.load(Ordering::Relaxed) {
            // The verification signal; see `exclusion_holds`. Rendered here, in
            // the presence stream, because that is the app's own output and
            // therefore exactly what a correct tap must not hear.
            let start = cycles.load(Ordering::Relaxed) as usize * out.len() / CHANNELS;
            for (i, sample) in out.iter_mut().enumerate() {
                let t = (start + i / CHANNELS) as f32 / SAMPLE_RATE as f32;
                *sample = (t * PROBE_HZ * std::f32::consts::TAU).sin() * PROBE_LEVEL;
            }
            return 0;
        }
        out.fill(0.0);
    }
    0
}

/// Set while [`exclusion_holds`] is measuring; makes the presence stream emit
/// the probe tone instead of silence.
static PROBING: AtomicBool = AtomicBool::new(false);

/// The probe's frequency, amplitude and length.
///
/// Loud enough to be unmistakable against anything else the machine is playing,
/// short enough to be a blip rather than a note. It *is* audible, once, when a
/// Desktop Audio source starts — which is the price of knowing the source is
/// safe rather than hoping.
const PROBE_HZ: f32 = 1_000.0;
const PROBE_LEVEL: f32 = 0.25;
const PROBE_FOR: Duration = Duration::from_millis(150);
/// Anything above this in the tap during the probe is our own output coming
/// back. Well under [`PROBE_LEVEL`], and well over the 0.02-ish a quiet machine
/// shows.
const PROBE_LIMIT: f32 = 0.08;

/// How much audio the tap has to have delivered before its silence means
/// anything. A tenth of a second, well past an aggregate device's start-up.
const WARMUP_SAMPLES: usize = SAMPLE_RATE as usize * CHANNELS / 10;

/// How long to wait for the presence stream to actually start rendering.
///
/// Generous, because being wrong in this direction costs a moment and being
/// wrong in the other costs the user broadcasting their own audio.
const PRESENCE_TIMEOUT: Duration = Duration::from_millis(2_000);

/// How many rendered cycles count as "the audio server has noticed us". See the
/// wait in [`acquire_presence`] for why this is not 1.
const PRESENCE_CYCLES: u64 = 12;

/// Opens the presence stream if this is the first tap to need it.
fn acquire_presence() -> Result<(), String> {
    let mut presence = PRESENCE.lock().unwrap_or_else(|e| e.into_inner());
    presence.1 += 1;
    if presence.0.is_some() {
        return Ok(());
    }
    let device = super::render::output_render_device().inspect_err(|_| {
        // Undo the count taken above, or a failed open leaves the refcount high
        // and the next release never closes the stream.
        presence.1 = presence.1.saturating_sub(1);
    })?;
    let cycles = Box::new(AtomicU64::new(0));
    // SAFETY: `cycles` is boxed and moved into the `Presence` below, which drops
    // its unit before its counter, so the pointer cannot outlive what it names.
    let unit = unsafe {
        super::coreaudio::open_output(
            device,
            Some(render_silence),
            std::ptr::from_ref(&*cycles) as *mut std::ffi::c_void,
        )
    };
    let unit = match unit {
        Ok(unit) => unit,
        Err(e) => {
            presence.1 = presence.1.saturating_sub(1);
            return Err(e);
        }
    };
    if let Err(e) = unit.start() {
        presence.1 = presence.1.saturating_sub(1);
        return Err(e);
    }

    // Wait for the stream to be genuinely established before returning, so the
    // tap created next binds its exclusion to a process the audio server already
    // knows is playing.
    //
    // **Cycles, not one cycle.** Waiting for the callback to fire once is not
    // enough: measured over eight runs, a tap created immediately after the
    // first cycle still captured our own output three times. `coreaudiod`
    // learns about a new audio client asynchronously, and the exclusion is
    // resolved against what it knows at that moment. A dozen cycles is about
    // 130 ms at a typical device period — long enough that it had settled in
    // every run since, and short enough to be invisible when a source starts.
    let deadline = Instant::now() + PRESENCE_TIMEOUT;
    while cycles.load(Ordering::Relaxed) < PRESENCE_CYCLES {
        if Instant::now() >= deadline {
            presence.1 = presence.1.saturating_sub(1);
            return Err(
                "the output device did not start, so Pubsplash's own audio could not be \
                 kept out of the capture"
                    .to_string(),
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    presence.0 = Some(Presence {
        _unit: unit,
        _cycles: cycles,
    });
    Ok(())
}

/// Closes the presence stream once the last tap has gone.
fn release_presence() {
    let mut presence = PRESENCE.lock().unwrap_or_else(|e| e.into_inner());
    presence.1 = presence.1.saturating_sub(1);
    if presence.1 == 0 {
        presence.0 = None;
    }
}

/// A live tap and the private aggregate device that carries it.
///
/// Dropping this destroys both. The order matters — the aggregate refers to the
/// tap — and is why `Drop` is written out rather than left to field order.
pub struct ProcessTap {
    device: AudioDeviceID,
    tap: AudioObjectID,
}

impl ProcessTap {

    /// A tap on exactly the given processes, for an Application source.
    pub fn including(processes: &[AudioObjectID]) -> Result<Self, String> {
        if processes.is_empty() {
            return Err("that application is not playing any audio".to_string());
        }
        Self::create(processes, false)
    }

    fn create(processes: &[AudioObjectID], exclude: bool) -> Result<Self, String> {
        // Before the tap, and that order is the whole point: the exclusion binds
        // to whatever is streaming when the tap is made. See `Presence`.
        acquire_presence()?;
        let numbers: Vec<Retained<NSNumber>> =
            processes.iter().map(|id| NSNumber::new_u32(*id)).collect();
        let list = NSArray::from_retained_slice(&numbers);

        // SAFETY: both initialisers take an `NSArray<NSNumber>` of process
        // object ids, which is what `list` is.
        let description = unsafe {
            if exclude {
                CATapDescription::initStereoGlobalTapButExcludeProcesses(
                    CATapDescription::alloc(),
                    &list,
                )
            } else {
                CATapDescription::initStereoMixdownOfProcesses(CATapDescription::alloc(), &list)
            }
        };
        // Named so that anything that does surface it — Console, a crash log —
        // says whose it is.
        // SAFETY: a plain setter on a live description.
        unsafe { description.setName(&NSString::from_str("Pubsplash capture")) };

        let mut tap: AudioObjectID = kAudioObjectUnknown;
        // SAFETY: `description` is live and `tap` is a local the call writes.
        let status = unsafe {
            AudioHardwareCreateProcessTap(Some(&description), std::ptr::from_mut(&mut tap))
        };
        if status != 0 || tap == kAudioObjectUnknown {
            release_presence();
            return Err(format!(
                "creating the audio tap: {} — this needs permission under \
                 System Settings > Privacy & Security > Audio Recording",
                status_message(status)
            ));
        }

        // SAFETY: reading the UUID of the description we just created a tap from.
        let tap_uid = unsafe { description.UUID() }.UUIDString().to_string();
        match Self::aggregate_for(&tap_uid) {
            Ok(device) => Ok(Self { device, tap }),
            Err(e) => {
                // The tap is ours and nothing else will free it.
                // SAFETY: `tap` is the id just created and is destroyed once.
                unsafe { AudioHardwareDestroyProcessTap(tap) };
                release_presence();
                Err(e)
            }
        }
    }

    /// Builds the private aggregate device that makes `tap_uid` readable.
    fn aggregate_for(tap_uid: &str) -> Result<AudioDeviceID, String> {
        /// The dictionary keys are C strings in the framework headers, and every
        /// one has to outlive the dictionary it is put in — hence the locals
        /// below rather than a helper called inline.
        fn key(k: &CStr) -> Retained<NSString> {
            NSString::from_str(&k.to_string_lossy())
        }

        let sub_tap_keys = [key(kAudioSubTapUIDKey), key(kAudioSubTapDriftCompensationKey)];
        let tap_uid = NSString::from_str(tap_uid);
        // Drift compensation on: the tap and the engine are on separate clocks,
        // which is the same mismatch the ring buffer absorbs for a microphone,
        // and letting Core Audio correct it here keeps the ring's occupancy flat
        // instead of ratcheting across a long broadcast.
        let yes = NSNumber::new_i32(1);
        let no = NSNumber::new_i32(0);
        let sub_tap = NSDictionary::from_slices(
            &[&*sub_tap_keys[0], &*sub_tap_keys[1]],
            &[&*tap_uid as &AnyObject, &*yes as &AnyObject],
        );

        // A UID of our own, so two Pubsplash sources never collide on one
        // aggregate and a stale one from a previous run is never adopted.
        let uid = NSString::from_str(&uuid::Uuid::new_v4().to_string());
        let name = NSString::from_str("Pubsplash capture");
        // No sub-devices: the tap is the only source. An aggregate with a real
        // device in it would also capture that device's *input*.
        let no_sub_devices = NSArray::<AnyObject>::new();
        let tap_list = NSArray::from_slice(&[&*sub_tap as &AnyObject]);

        let keys = [
            key(kAudioAggregateDeviceNameKey),
            key(kAudioAggregateDeviceUIDKey),
            key(kAudioAggregateDeviceIsPrivateKey),
            key(kAudioAggregateDeviceIsStackedKey),
            key(kAudioAggregateDeviceTapAutoStartKey),
            key(kAudioAggregateDeviceSubDeviceListKey),
            key(kAudioAggregateDeviceTapListKey),
        ];
        let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
        let values: [&AnyObject; 7] = [
            &*name,
            &*uid,
            // Private: see the module header. A visible aggregate would show up
            // in every other app's device list and outlive a crash.
            &*yes,
            &*no,
            &*yes,
            &*no_sub_devices,
            &*tap_list,
        ];
        let description = NSDictionary::from_slices(&key_refs, &values);

        let mut device: AudioDeviceID = kAudioObjectUnknown;
        // SAFETY: an `NSDictionary` *is* a `CFDictionary` — the two are toll-free
        // bridged — and this one is in the shape the API documents. `device` is a
        // local the call writes on success.
        let status = unsafe {
            AudioHardwareCreateAggregateDevice(
                &*(std::ptr::from_ref(&*description).cast::<CFDictionary>()),
                std::ptr::NonNull::from(&mut device),
            )
        };
        if status != 0 || device == kAudioObjectUnknown {
            return Err(format!(
                "creating the capture device: {}",
                status_message(status)
            ));
        }
        set_engine_rate(device);
        Ok(device)
    }

    /// The device id to open an input `AudioUnit` on.
    pub fn device(&self) -> AudioDeviceID {
        self.device
    }
}

impl Drop for ProcessTap {
    fn drop(&mut self) {
        // The aggregate first: it refers to the tap, and destroying the tap out
        // from under it leaves the HAL holding a dangling sub-tap.
        // SAFETY: both ids are ours, were created together, and are destroyed
        // exactly once.
        unsafe {
            AudioHardwareDestroyAggregateDevice(self.device);
            AudioHardwareDestroyProcessTap(self.tap);
        }
        release_presence();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case the whole `tree_of` function exists for: a browser whose audio
    /// comes out of a grandchild utility process, not the window owner.
    #[test]
    fn a_process_tree_reaches_grandchildren() {
        let processes = [
            (100, None),      // the browser
            (101, Some(100)), // a renderer
            (102, Some(101)), // its audio utility child
            (200, None),      // something else entirely
            (201, Some(200)),
        ];

        let mut tree = tree_of(100, &processes);
        tree.sort_unstable();

        assert_eq!(tree, vec![100, 101, 102]);
    }

    #[test]
    fn a_process_with_no_children_is_its_own_tree() {
        assert_eq!(tree_of(42, &[(42, None), (7, None)]), vec![42]);
    }

    /// A root that is not in the snapshot at all still taps itself: the process
    /// list is a moment old and a source should not vanish because of it.
    #[test]
    fn an_unknown_root_still_taps_itself() {
        assert_eq!(tree_of(42, &[(1, None)]), vec![42]);
    }

    /// A parent cycle cannot happen on a healthy system, but the snapshot is
    /// read without locking and a hang here would take the capture thread with
    /// it. The loop must terminate whatever it is handed.
    #[test]
    fn a_parent_cycle_terminates() {
        let processes = [(1, Some(2)), (2, Some(1)), (3, Some(1))];

        let tree = tree_of(1, &processes);

        assert!(tree.contains(&1));
        assert!(tree.len() <= processes.len() + 1);
    }



    /// The device's nominal rate, for the diagnostic above.
    fn nominal_rate(device: AudioDeviceID) -> Option<f64> {
        use objc2_core_audio::kAudioDevicePropertyNominalSampleRate;
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyNominalSampleRate,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut rate = 0f64;
        let mut size = std::mem::size_of::<f64>() as u32;
        // SAFETY: `rate` and `size` are live locals the call writes.
        let status = unsafe {
            AudioObjectGetPropertyData(
                device,
                std::ptr::NonNull::from(&address),
                0,
                std::ptr::null(),
                std::ptr::NonNull::from(&mut size),
                std::ptr::NonNull::from(&mut rate).cast(),
            )
        };
        (status == 0).then_some(rate)
    }

    /// Nothing to tap is an error rather than a tap on everything, which is what
    /// an empty include list would otherwise mean to Core Audio.
    #[test]
    fn including_nothing_is_refused() {
        assert!(ProcessTap::including(&[]).is_err());
    }

}
