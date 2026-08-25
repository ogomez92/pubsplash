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

/// Core Audio playback. **Not built yet** — this is the platform seam standing
/// open, not a design.
///
/// What goes here is an `AudioUnit` of subtype `HALOutput` bound to the chosen
/// device, or an `AudioQueue`, fed from a render callback; device resolution is
/// `AudioObjectGetPropertyData` with `kAudioHardwarePropertyDefaultOutputDevice`
/// for the `None` case and a `kAudioDevicePropertyDeviceUID` match for a
/// configured one. The `Active`-state rule the Windows side documents has a
/// direct equivalent worth keeping: a device whose UID no longer resolves must
/// be an error, never a fall back to the default, for exactly the feedback
/// reason given above.
///
/// Until then every cue and preview reports that it could not play. The callers
/// all log rather than open a dialog, so this is a quiet line in the log per
/// sound rather than an unusable app — and it is deliberately an `Err` and not a
/// silent `Ok`, so that nothing later reads "the cue played" from this.
#[cfg(target_os = "macos")]
mod imp {
    use std::sync::atomic::AtomicBool;

    /// Stands in for a Core Audio `AudioDeviceID`. Nothing constructs one yet.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Device(pub u32);

    const UNBUILT: &str = "audio output is not implemented on macOS yet";

    pub fn output_render_device() -> Result<Device, String> {
        Err(UNBUILT.to_string())
    }

    pub fn play_samples_until(_samples: &[f32], _stop: &AtomicBool) -> Result<(), String> {
        Err(UNBUILT.to_string())
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
    /// Windows-only for now only because the macOS side is not built; when it
    /// is, this test is one of the things it has to satisfy.
    #[cfg(windows)]
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
}
