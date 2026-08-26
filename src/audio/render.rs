//! Playing a buffer of engine-format samples out of Pubsplash's chosen render
//! device, with nothing else attached.
//!
//! This is the bottom half of `cue.rs`, split out for the same reason
//! `convert.rs` was: the standalone Sound Pack Manager has to preview the file
//! an author just picked, and its preview used to be a PowerShell
//! `System.Media.SoundPlayer` shell-out, which plays WAV and nothing else — no
//! use at all once a pack may hold Opus. So this file is `#[path]`-included
//! there, and like `convert.rs` it must name nothing from this crate: no
//! `crate::`, no `super::`. That is why the two constants below are spelled out
//! rather than imported from `convert`, and why the device is opened here
//! instead of through `audio::device`.
//!
//! The platform seam is the two functions at the bottom, [`output_render_device`]
//! and the render loop behind [`play_samples_until`]. Everything above them —
//! the setting, its lock, the sample-to-bytes conversion — is portable and
//! shared, and so are the tests for it.
//!
//! That isolation is also why the *output device setting* lives here rather
//! than in `audio::device`, where it would otherwise belong: Pubsplash plays
//! out of exactly one endpoint through two independent paths — the mixer's
//! monitoring tap (`audio::monitor`) and the local cues below — and only one of
//! those two files may name the crate. So this one owns
//! [`OUTPUT_DEVICE`] and [`output_render_device`], and `audio::device` and
//! `audio::monitor` reach *in* here, never the reverse. The standalone binaries
//! never call [`set_output_device`], so they keep following the system default.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The Core Audio primitives, reached by `#[path]` rather than by naming the
/// crate — this file may not do that (see the header).
///
/// The relative path works from both of the places this file is loaded from
/// because a child module resolves against the directory holding the file that
/// declares it, which is this one: `src/audio/` when the crate loads it, and
/// `src/bin/../audio/` when the soundpack manager does. Those are two spellings
/// of one directory, so `coreaudio.rs` finds the same file either way. (The
/// same is not true one level down — a child of `ca` would resolve against a
/// directory named after `ca` itself — so `coreaudio.rs` must stay a leaf.)
///
/// The cost is that `coreaudio.rs` is compiled twice in the main binary, once
/// here and once as `audio::coreaudio` — the same trade `soundpack.rs` already
/// makes with `convert.rs`, and for the same reason.
#[cfg(target_os = "macos")]
#[allow(clippy::duplicate_mod)]
#[path = "coreaudio.rs"]
mod ca;

/// The handle an opened output endpoint is named by: a `wasapi::Device` on
/// Windows, and a Core Audio device id on macOS.
pub use imp::Device;

/// Must match `convert::ENGINE_SAMPLE_RATE`; every buffer reaching here has
/// already been converted to it.
const SAMPLE_RATE: u32 = 48_000;
/// Must match `convert::ENGINE_CHANNELS`.
const CHANNELS: usize = 2;

/// How long to keep the device open after the last sample was handed over, so
/// the tail of a cue is not cut off by the stream closing under it.
const DRAIN_AFTER_CUE: Duration = Duration::from_millis(100);

/// The endpoint id Pubsplash plays out of, or `None` to follow whatever the
/// system currently calls the default. Set once at startup from the saved
/// settings and again whenever the user changes it in Preferences.
///
/// A process-global rather than a parameter because the two callers are on
/// different threads and neither is reachable from the other: the monitoring
/// thread, which the engine spawns and drops on its own schedule, and every
/// one-shot cue thread. Both read it at the moment they open a device, so a
/// change reaches the next cue immediately; the monitoring thread is asked to
/// reopen by `EngineCommand::ReopenMonitor`.
static OUTPUT_DEVICE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn output_device() -> &'static Mutex<Option<String>> {
    OUTPUT_DEVICE.get_or_init(|| Mutex::new(None))
}

/// Locks [`OUTPUT_DEVICE`], recovering from poisoning: the alternative is that
/// one panic anywhere leaves every later cue and every reopened monitor unable
/// to find a device at all, which is silence with no explanation.
fn lock_output_device() -> std::sync::MutexGuard<'static, Option<String>> {
    let mutex = output_device();
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::error!("The output device setting was poisoned by an earlier panic; recovering");
            mutex.clear_poison();
            poisoned.into_inner()
        }
    }
}

/// Chooses the endpoint Pubsplash plays out of. `None` follows the system
/// default.
pub fn set_output_device(id: Option<String>) {
    *lock_output_device() = id;
}

/// What [`set_output_device`] was last given.
pub fn output_device_id() -> Option<String> {
    lock_output_device().clone()
}

/// Opens the endpoint Pubsplash plays out of.
///
/// The rules are `audio::device::capture_device`'s, deliberately: a configured
/// device that is not active is an error the caller retries, never a silent swap
/// for the default one. That matters more here than it looks. `audio::capture`
/// refuses to point a Desktop Audio source at the endpoint this function
/// returns, because endpoint loopback would capture Pubsplash's own speech and
/// cues straight back into the stream — and a silent fallback to the default
/// device could land the output on exactly the endpoint that check just
/// cleared. Failing instead keeps the two answers in agreement.
pub fn output_render_device() -> Result<Device, String> {
    imp::output_render_device()
}

/// Plays `samples` to the end, blocking until it has drained.
pub fn play_samples(samples: &[f32]) -> Result<(), String> {
    play_samples_until(samples, &AtomicBool::new(false))
}

/// The same, but stoppable: `stop` is read at the top of every render block.
///
/// A stop deliberately skips the [`DRAIN_AFTER_CUE`] wait. That wait exists so
/// a cue that reached its end is not cut off by the device closing under it,
/// and a stop is the user asking for exactly that cut-off.
pub fn play_samples_until(samples: &[f32], stop: &AtomicBool) -> Result<(), String> {
    // Checked before the device is opened, not only in the loop: a playback
    // stopped this early should cost nothing, which is also what makes the
    // behaviour testable without an audio device.
    if samples.is_empty() || stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    imp::play_samples_until(samples, stop)
}

#[cfg(windows)]
mod imp {
    use super::{
        CHANNELS, DRAIN_AFTER_CUE, SAMPLE_RATE, append_frames, output_device_id,
    };
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;
    use wasapi::{DeviceEnumerator, DeviceState, Direction, SampleType, StreamMode, WaveFormat};

    pub use wasapi::Device;

    pub fn output_render_device() -> Result<Device, String> {
        let _ = wasapi::initialize_mta();
        let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
        let Some(id) = output_device_id() else {
            return enumerator
                .get_default_device(&Direction::Render)
                .map_err(|e| format!("finding the default playback device: {e}"));
        };
        let device = enumerator
            .get_device(&id)
            .map_err(|e| format!("looking up the configured playback device: {e}"))?;
        match device.get_state() {
            Ok(DeviceState::Active) => Ok(device),
            Ok(state) => Err(format!("the configured playback device is {state:?}")),
            Err(e) => Err(format!(
                "reading the configured playback device's state: {e}"
            )),
        }
    }

    pub fn play_samples_until(samples: &[f32], stop: &AtomicBool) -> Result<(), String> {
        let format = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            SAMPLE_RATE as usize,
            CHANNELS,
            None,
        );

        let mut client = output_render_device()?
            .get_iaudioclient()
            .map_err(|e| format!("activating the playback device's audio client: {e}"))?;

        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: 0,
        };
        client
            .initialize_client(&format, &Direction::Render, &mode)
            .map_err(|e| format!("initializing the playback stream: {e}"))?;

        let event = client
            .set_get_eventhandle()
            .map_err(|e| format!("setting up the playback event: {e}"))?;
        let render = client
            .get_audiorenderclient()
            .map_err(|e| format!("getting the render client: {e}"))?;
        let blockalign = format.get_blockalign() as usize;

        client
            .start_stream()
            .map_err(|e| format!("starting the playback stream: {e}"))?;

        let mut offset = 0;
        let mut bytes = VecDeque::new();
        let mut finished_writing_at: Option<Instant> = None;

        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if finished_writing_at.is_some_and(|finished| finished.elapsed() >= DRAIN_AFTER_CUE) {
                break;
            }

            let frames = client
                .get_available_space_in_frames()
                .map_err(|e| format!("reading the available playback space: {e}"))?
                as usize;
            if frames > 0 {
                bytes.clear();
                bytes.reserve(frames * blockalign);
                append_frames(&mut bytes, samples, &mut offset, frames);
                render
                    .write_to_device_from_deque(frames, &mut bytes, None)
                    .map_err(|e| format!("writing to the playback device: {e}"))?;
                if offset >= samples.len() && finished_writing_at.is_none() {
                    finished_writing_at = Some(Instant::now());
                }
            }
            let _ = event.wait_for_event(200);
        }

        let _ = client.stop_stream();
        Ok(())
    }
}

/// Core Audio playback, through a HAL output unit fed from a render callback.
///
/// The Windows side writes into the device's buffer from a loop it owns; Core
/// Audio inverts that — the HAL calls us on its own real-time thread whenever it
/// wants more. So the shape here is: park a cursor over the sample slice where
/// the callback can reach it, start the unit, and sleep until the callback says
/// it is done or the caller sets `stop`.
///
/// **Nothing in the callback allocates, locks or logs.** It runs on a real-time
/// thread with a deadline, and a priority inversion there is a glitch in the
/// output. It reads a slice, writes a slice, and bumps two atomics.
#[cfg(target_os = "macos")]
mod imp {
    use super::{DRAIN_AFTER_CUE, ca, output_device_id};
    use objc2_audio_toolbox::AudioUnitRenderActionFlags;
    use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// A Core Audio device id. Named `Device` so both platforms' callers read
    /// alike.
    pub type Device = u32;

    pub fn output_render_device() -> Result<Device, String> {
        match output_device_id() {
            Some(uid) => {
                let device = ca::device_for_uid(&uid)
                    .ok_or("the configured playback device is not connected")?;
                if !ca::has_streams(device, ca::Scope::Output) {
                    return Err("the configured playback device has no outputs".to_string());
                }
                Ok(device)
            }
            None => ca::default_device(ca::Scope::Output)
                .ok_or_else(|| "there is no default playback device".to_string()),
        }
    }

    /// What the render callback reads. Shared with the HAL's real-time thread,
    /// so every field is either immutable for the unit's lifetime or an atomic.
    struct Playback<'a> {
        samples: &'a [f32],
        /// How far through `samples` the callback has got.
        cursor: AtomicUsize,
        /// Set by the callback once it has handed over the last sample, so the
        /// waiting thread knows to start the drain.
        finished: AtomicBool,
    }

    /// The HAL's render callback. Real-time thread; see the module header.
    ///
    /// # Safety
    /// `ref_con` is the `Playback` passed to `open_output`, alive for the life
    /// of the unit. `io_data` is the HAL's buffer list for this cycle.
    unsafe extern "C-unwind" fn render(
        ref_con: NonNull<c_void>,
        _flags: NonNull<AudioUnitRenderActionFlags>,
        _time: NonNull<AudioTimeStamp>,
        _bus: u32,
        frames: u32,
        io_data: *mut AudioBufferList,
    ) -> i32 {
        // SAFETY: both pointers are the HAL's and ours respectively, and both
        // outlive this call by the contract above.
        let playback = unsafe { &*ref_con.as_ptr().cast::<Playback>() };
        let Some(out) = (unsafe { ca::hal_buffer(io_data, frames) }) else {
            return 0;
        };

        // Everything below is bounded by `out.len()`, never by `frames`. The two
        // are normally the same, but `ca::hal_buffer` deliberately shortens the
        // slice when the HAL's own two accounts of the buffer disagree, and a
        // panic here would unwind into a real-time C callback.
        let start = playback.cursor.load(Ordering::Relaxed);
        let available = playback.samples.len().saturating_sub(start);
        let taken = available.min(out.len());
        out[..taken].copy_from_slice(&playback.samples[start..start + taken]);
        // Whatever is left of the cycle is silence. The HAL does not zero the
        // buffer for us, and handing back the previous cycle's contents is a
        // loud buzz rather than a quiet end.
        out[taken..].fill(0.0);

        let cursor = start + taken;
        playback.cursor.store(cursor, Ordering::Relaxed);
        // The end of the samples, not a short cycle: a cycle can come up short
        // for the buffer reason above with audio still to play, and taking that
        // for the end would start the drain early and clip the cue's tail.
        if cursor == playback.samples.len() {
            playback.finished.store(true, Ordering::Relaxed);
        }
        0
    }

    pub fn play_samples_until(samples: &[f32], stop: &AtomicBool) -> Result<(), String> {
        let device = output_render_device()?;
        let playback = Playback {
            samples,
            cursor: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
        };
        // SAFETY: `playback` outlives `unit` -- it is declared first and `unit`
        // is dropped at the end of this scope, and `Unit::drop` stops the
        // callback before returning.
        let unit = unsafe {
            ca::open_output(
                device,
                Some(render),
                std::ptr::from_ref(&playback) as *mut c_void,
            )?
        };
        unit.start()?;

        // Polled rather than woken, which is the one place this file does that
        // and is deliberate. The Windows side above waits on a real WASAPI event
        // because the app owns the writing thread there; here the HAL owns it,
        // and the only thing that knows a cue has ended is the callback. Having
        // it signal a `Condvar` would put a mutex on a real-time thread, and the
        // moment this thread held that mutex the audio thread would block on a
        // normal-priority one — the priority inversion the module header exists
        // to forbid, paid for as a glitch in the output. A 10 ms tick on a
        // thread that is otherwise asleep is the cheaper side of that trade: the
        // callback does the work, this only notices when it stopped.
        const TICK: Duration = Duration::from_millis(10);
        let mut finished_at: Option<Instant> = None;
        loop {
            if stop.load(Ordering::Relaxed) {
                // A stop deliberately skips the drain: that wait exists so a cue
                // that reached its end is not cut off, and a stop is the user
                // asking for exactly that cut-off.
                break;
            }
            if playback.finished.load(Ordering::Relaxed) {
                let since = *finished_at.get_or_insert_with(Instant::now);
                if since.elapsed() >= DRAIN_AFTER_CUE {
                    break;
                }
            }
            std::thread::sleep(TICK);
        }
        // Explicit, so the order is visible: the unit stops (and with it the
        // callback) before `playback` goes out of scope.
        drop(unit);
        Ok(())
    }
}

fn append_frames(bytes: &mut VecDeque<u8>, samples: &[f32], offset: &mut usize, frames: usize) {
    for _ in 0..frames * CHANNELS {
        let sample = samples
            .get(*offset)
            .copied()
            .unwrap_or(0.0)
            .clamp(-1.0, 1.0);
        if *offset < samples.len() {
            *offset += 1;
        }
        bytes.extend(sample.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_frames_clamps_and_pads() {
        let mut bytes = VecDeque::new();
        let mut offset = 0;

        append_frames(&mut bytes, &[2.0, -2.0], &mut offset, 2);

        let out: Vec<f32> = bytes
            .into_iter()
            .collect::<Vec<_>>()
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(out, vec![1.0, -1.0, 0.0, 0.0]);
        assert_eq!(offset, 2);
    }

    /// An already-stopped playback must return before it touches WASAPI, which
    /// is both the point of the early check and the only reason this test can
    /// run on a machine with no audio device.
    #[test]
    fn an_already_stopped_playback_opens_no_device() {
        let stop = AtomicBool::new(true);

        assert!(play_samples_until(&[0.5, 0.5, -0.5, -0.5], &stop).is_ok());
    }

    /// The setting is a process-global, so this test restores whatever it found
    /// rather than assuming it started empty — `cargo test` runs the whole
    /// module in one process.
    #[test]
    fn the_output_device_setting_round_trips() {
        let previous = output_device_id();

        set_output_device(Some("{some-endpoint-id}".to_string()));
        assert_eq!(output_device_id().as_deref(), Some("{some-endpoint-id}"));
        set_output_device(None);
        assert_eq!(output_device_id(), None);

        set_output_device(previous);
    }

    /// A configured device that does not exist must be an error, never a quiet
    /// fallback to the default one: `audio::capture` clears a Desktop Audio
    /// source against the endpoint this returns, and a fallback could put the
    /// output back onto the endpoint that check just approved.
    ///
    /// Both platforms: a Core Audio UID that resolves to no device has to fail
    /// for the same reason a WASAPI endpoint id does.
    #[test]
    fn an_unknown_output_device_is_an_error_not_the_default() {
        let previous = output_device_id();

        set_output_device(Some("{not-a-real-endpoint}".to_string()));
        let opened = output_render_device();

        set_output_device(previous);
        assert!(
            opened.is_err(),
            "an unknown endpoint id must not resolve to a device"
        );
    }

    /// A configured device must resolve to *that* device. The other half of the
    /// test above: refusing an unknown id is only right if a known one is
    /// honoured, and the two paths through `output_render_device` — follow the
    /// system default, or match a saved UID — are otherwise unrelated code.
    ///
    /// Machine-independent because it asks for the default device by its own
    /// UID, so the two paths must agree on a device this machine really has.
    /// Skips itself where there is no output device at all, which is what CI is.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_configured_device_resolves_to_that_device() {
        let previous = output_device_id();

        set_output_device(None);
        let Ok(default) = output_render_device() else {
            return;
        };
        let uid = ca::device_uid(default).expect("a live device should have a UID");

        set_output_device(Some(uid.clone()));
        let configured = output_render_device();

        set_output_device(previous);
        assert_eq!(
            configured,
            Ok(default),
            "asking for {uid} should reach the device it names"
        );
    }

    /// Plays a second of a 440 Hz tone out of the default device.
    ///
    /// Ignored because it makes a noise, and there is nothing here to assert
    /// against: whether the HAL unit is wired up correctly is a question about
    /// pitch, channel count and clicks that only an ear can answer. Run it with
    /// `cargo test plays_a_tone -- --include-ignored --nocapture`. A tone at the
    /// wrong pitch means the sample rate is not being negotiated; hearing it in
    /// one ear means the format went across as non-interleaved; a click at the
    /// end means the drain is too short.
    #[test]
    #[ignore = "plays audio"]
    fn plays_a_tone_out_of_the_default_device() {
        let frames = SAMPLE_RATE as usize;
        let mut samples = Vec::with_capacity(frames * CHANNELS);
        for frame in 0..frames {
            let t = frame as f32 / SAMPLE_RATE as f32;
            // Faded in and out over 20 ms, so a click at either end is the
            // device's doing rather than the test's.
            let fade = (t / 0.02).min((1.0 - t) / 0.02).clamp(0.0, 1.0);
            let value = (t * 440.0 * std::f32::consts::TAU).sin() * 0.2 * fade;
            samples.push(value);
            samples.push(value);
        }

        play_samples(&samples).expect("the default device should play a tone");
    }
}
