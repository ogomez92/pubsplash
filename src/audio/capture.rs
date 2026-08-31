//! Capture threads: each audio source that reads from the OS runs one of
//! these, producing interleaved stereo f32 at 48 kHz into a ring buffer the
//! mixer drains.
//!
//! **Which side of the ring drives is where the two platforms differ**, exactly
//! as in [`crate::audio::monitor`], and it decides the shape of each `imp`. On
//! Windows the app owns the loop: it waits on a WASAPI event, drains every
//! packet the device has, and leans on [`StallGuard`] to tell a normal timeout
//! from a wait that has stopped waiting. On macOS the HAL owns the thread and
//! calls *us*, so there is no loop and no wait to guard — only a callback, and a
//! supervising thread watching a pulse the callback bumps. Hence two
//! ring-filling functions rather than one, [`push_f32`] and [`push_samples`],
//! which are the same discard rule written for a thread the app owns and for a
//! real-time thread it does not.
// Items below are reached only from the Windows `imp` in this file. They are not
// dead in the codebase, only unreached on macOS, and each is wanted the moment
// the other platform is built -- so this is scoped to the file rather than being
// a crate-wide allow.
//
// Two groups, and they come off at different times. `StallGuard` and its three
// constants are Windows' way of noticing a device that has stopped working, and
// have no macOS counterpart at all: there the HAL owns the thread, so the `imp`
// below watches a callback pulse instead (see its header). `push_f32`,
// `silence_appended` and `would_capture_pubsplash` are waiting on the process-tap
// work -- `would_capture_pubsplash` in particular is called only from the Desktop
// Audio paths, which macOS does not have yet.
#![cfg_attr(not(windows), allow(dead_code))]


use crate::audio::device;
use crate::audio::health::CaptureStats;
use crate::audio::mixer::CHANNELS;
// Only the WASAPI side asks the device for a format; Core Audio's client
// format is set in `coreaudio::engine_format`.
#[cfg(windows)]
use crate::audio::mixer::SAMPLE_RATE;
use rtrb::Producer;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureKind {
    Microphone {
        device_id: Option<String>,
    },
    /// See the three cases in [`open_desktop_audio`]: `None` is the
    /// all-endpoints form that excludes Pubsplash's own audio, `Some` pins one
    /// render endpoint.
    DesktopAudio {
        device_id: Option<String>,
    },
    Application {
        pid: u32,
    },
}

/// What a capture thread reports about itself, whenever the answer changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureState {
    /// The device opened and audio is flowing.
    Running,
    /// The device could not be opened, or stopped working. The thread is
    /// retrying; the string says which step failed.
    Failed(String),
    /// This source cannot run on this build at all, so no retry is scheduled and
    /// the thread has ended. The string says why, in the user's terms.
    ///
    /// **Distinct from [`Self::Failed`] because the two ask the user for
    /// opposite things.** A failure is worth waiting out; this is not, and a
    /// source that says "reconnecting" forever is worse than one that says what
    /// is wrong -- it sends somebody looking for a flapping device that was
    /// never there.
    Unavailable(String),
}

/// One report from a capture thread. `epoch` is the source-set generation the
/// thread was spawned for: threads outlive their `SetSources` (they retire
/// asynchronously, and a retrying one may be sleeping), so the engine uses this
/// to drop reports from threads that no longer own their source's name.
#[derive(Debug, Clone)]
pub struct CaptureReport {
    pub name: String,
    pub epoch: u64,
    pub state: CaptureState,
}

/// How long to wait before the next attempt to open a device, after `attempt`
/// consecutive failures. Quick at first, since the common case is an interface
/// that is a few hundred milliseconds late to enumerate at launch, then slow
/// enough that a device which is simply gone costs nothing to keep waiting for.
fn backoff(attempt: u32) -> Duration {
    const SCHEDULE_MS: [u64; 5] = [250, 500, 1_000, 2_000, 5_000];
    let index = (attempt as usize).min(SCHEDULE_MS.len() - 1);
    Duration::from_millis(SCHEDULE_MS[index])
}

/// How long the device event is waited on per loop turn, in milliseconds. Short
/// enough that a retiring thread releases the endpoint promptly — `stop` is only
/// checked at the top of the loop, and a replacement thread is spawned for the
/// same device the instant `SetSources` lands, so a long wait here means the two
/// overlap on the device.
pub const WAIT_MS: u32 = 200;

/// A failed wait that returned this quickly did not wait at all.
const IMMEDIATE: Duration = Duration::from_millis(50);

/// How many immediate failures in a row before the wait is declared broken.
/// Twenty is about a second of spinning — long enough that a burst of scheduler
/// noise cannot reach it, short enough that the reopen happens while the user is
/// still wondering why the source went quiet.
const STALL_LIMIT: u32 = 20;

/// Tracks a wait that keeps failing without waiting.
///
/// `wasapi 0.23`'s `Handle::wait_for_event` maps *every* non-signalled return to
/// the same `WasapiError::EventTimeout` — `WAIT_TIMEOUT` and `WAIT_FAILED`
/// alike — and does not expose the raw handle, so there is no way to ask which
/// one happened. But the two do not look alike from outside: a real timeout
/// takes the full budget, while a dead handle fails instantly. A run of instant
/// failures is therefore a wait that will never work again, and the loop around
/// it is spinning a core rather than waiting.
///
/// Counting them turns that into an error the caller's existing reopen-with-
/// backoff loop already knows how to handle.
#[derive(Default)]
pub struct StallGuard {
    immediate_failures: u32,
}

impl StallGuard {
    /// Runs one wait. `wait` reports whether the event was signalled; the
    /// elapsed time decides how a `false` is read.
    ///
    /// Returns `Err` once the wait has failed instantly [`STALL_LIMIT`] times
    /// running, naming `what` in the message.
    pub fn wait(&mut self, what: &str, wait: impl FnOnce() -> bool) -> Result<(), String> {
        let started = std::time::Instant::now();
        if wait() {
            self.immediate_failures = 0;
            return Ok(());
        }
        if started.elapsed() >= IMMEDIATE {
            // A genuine timeout, which is normal: loopback with nothing playing
            // times out every turn forever.
            self.immediate_failures = 0;
            return Ok(());
        }
        self.immediate_failures += 1;
        if self.immediate_failures >= STALL_LIMIT {
            self.immediate_failures = 0;
            return Err(format!("waiting on the {what} event stopped working"));
        }
        Ok(())
    }
}

/// Sleeps up to `total`, waking early once `stop` is set.
fn sleep_interruptibly(total: Duration, stop: &AtomicBool) {
    const SLICE: Duration = Duration::from_millis(50);
    let deadline = std::time::Instant::now() + total;
    while !stop.load(Ordering::Relaxed) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(SLICE));
    }
}

/// Spawns a capture thread. It runs until `stop` is set, reopening the device
/// with a backoff whenever it fails, so a source that was not ready at launch
/// (or that is unplugged mid-session) recovers on its own. Returns the join
/// handle; state changes are logged and reported through `on_state`.
pub fn spawn(
    name: String,
    epoch: u64,
    kind: CaptureKind,
    mut producer: Producer<f32>,
    stop: Arc<AtomicBool>,
    on_state: crossbeam_channel::Sender<CaptureReport>,
    stats: Arc<CaptureStats>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("capture-{name}"))
        .spawn(move || {
            device::ensure_com_initialized();
            let report = |state: CaptureState| {
                let _ = on_state.send(CaptureReport {
                    name: name.clone(),
                    epoch,
                    state,
                });
            };
            // Asked once, before any attempt: whether this source kind can run
            // on this build at all. A configuration the platform does not
            // support is not a device that might come back, so it is reported
            // and the thread ends rather than backing off forever.
            if let Some(why) = imp::unsupported(&kind) {
                log::error!("Capture source {name:?} cannot run: {why}");
                report(CaptureState::Unavailable(why));
                return;
            }
            // Counts consecutive failures, so a device that flaps does not
            // flood the log and a device that recovers gets a fast retry again.
            let mut failures: u32 = 0;
            while !stop.load(Ordering::Relaxed) {
                let opened = std::cell::Cell::new(false);
                // Coming back from a failure is worth a line; opening normally
                // at launch is not.
                let recovering = failures > 0;
                let started = || {
                    opened.set(true);
                    if recovering {
                        log::info!("Capture source {name:?} is running again");
                    } else {
                        log::debug!("Capture source {name:?} is running");
                    }
                    report(CaptureState::Running);
                };
                let outcome = run(&kind, &mut producer, &stop, &stats, started);
                // A run that got as far as producing audio starts the backoff
                // over, so a device lost mid-session is retried as promptly as
                // one that was late to appear at launch — and its next failure
                // is news again.
                if opened.get() {
                    failures = 0;
                }
                match outcome {
                    // The stop flag was set: this source is being retired.
                    Ok(()) => return,
                    Err(e) => {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        // Only the first failure of a run is news; the retries
                        // after it say the same thing every few seconds.
                        if failures == 0 {
                            log::error!("Capture source {name:?} failed: {e}; retrying");
                            report(CaptureState::Failed(e));
                        } else {
                            log::debug!("Capture source {name:?} still failing: {e}");
                        }
                        sleep_interruptibly(backoff(failures), &stop);
                        failures = failures.saturating_add(1);
                    }
                }
            }
        })
        .expect("spawning capture thread")
}

/// Opens the device `kind` names and pumps it into `producer` until `stop` is
/// set, calling `started` once it is actually delivering audio.
///
/// **This is the platform seam of the whole capture path.** Everything above it
/// — the supervisor loop, the backoff schedule, the state reporting, the stall
/// guard — is portable and stays put; everything below it is the OS's capture
/// API and its particular set of hazards.
fn run(
    kind: &CaptureKind,
    producer: &mut Producer<f32>,
    stop: &AtomicBool,
    stats: &CaptureStats,
    started: impl FnOnce(),
) -> Result<(), String> {
    imp::run(kind, producer, stop, stats, started)
}

/// Whether a Desktop Audio source pinned to `device_id` would capture
/// Pubsplash's own output, which would feed our speech and cues back into the
/// stream.
///
/// Asked at every open and not only in the dialog, because the output device
/// can change afterwards — and a `None` output setting follows the *system*
/// default, which moves when a headset is plugged in. An unknown effective
/// output device answers `true`: the caller must read that as "cannot rule out
/// a collision", never as "no collision".
///
/// Unused on Windows, where the only caller is the `imp` below and reaches its
/// own copy directly. The Desktop Audio dialog deliberately asks a *narrower*
/// question inline (`scenes.rs`): it refuses only a device it can prove is the
/// output one, because an unknown effective output must not make the picker
/// reject every device with a message naming one. The fail-safe `None` case
/// belongs to the open path, which can fall back to all-endpoints silently.
#[cfg_attr(windows, allow(dead_code))]
pub fn would_capture_pubsplash(device_id: &str) -> bool {
    imp::would_capture_pubsplash(device_id)
}

#[cfg(windows)]
mod imp {
    use super::{
        CHANNELS, CaptureKind, SAMPLE_RATE, StallGuard, WAIT_MS, device, push_f32,
        silence_appended,
    };
    use crate::audio::health::{CaptureStats, DeviceTimeline};
    use rtrb::Producer;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};

    /// Why this source kind cannot run here, or `None` if it can.
    ///
    /// Always `None` on Windows: every source kind the app offers has a WASAPI
    /// form, so a failure here is always a device that might come back. The
    /// macOS twin is where this earns its keep.
    pub fn unsupported(_kind: &CaptureKind) -> Option<String> {
        None
    }

    /// How a device is waited on between reads.
    ///
    /// Endpoint loopback (a Desktop Audio source pinned to one render device) does
    /// not get an event: `AUDCLNT_STREAMFLAGS_LOOPBACK` and
    /// `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` do not work together — the endpoint is
    /// clocked by whatever is *playing* on it, so with nothing playing there is
    /// nothing to raise the event, and the wait can block for the life of the
    /// source. Every other form is event-driven exactly as before.
    ///
    /// [`StallGuard`] is unaffected by the polling arm: a wait that always reports
    /// success never counts an immediate failure, which is the right answer —
    /// a poll that returns nothing is not a wait that has stopped working.
    enum Pump {
        Event(wasapi::Handle),
        Poll(Duration),
    }

    impl Pump {
        /// Waits one turn, reporting whether there is any reason to think data
        /// arrived. A [`Pump::Poll`] always says yes: it slept its full interval,
        /// and the caller finds out by asking for the next packet size.
        fn wait(&self) -> bool {
            match self {
                Pump::Event(event) => event.wait_for_event(WAIT_MS).is_ok(),
                Pump::Poll(interval) => {
                    std::thread::sleep(*interval);
                    true
                }
            }
        }
    }

    /// How often a polled (endpoint-loopback) capture checks for new audio. One
    /// mixer block, so the ring is fed at the cadence it is drained at — but this
    /// is a floor, not a promise: see [`POLL_BUFFER_HNS`].
    const POLL_INTERVAL: Duration = Duration::from_millis(10);

    /// The capture buffer a polled endpoint-loopback stream asks for, in 100 ns
    /// units — 200 ms.
    ///
    /// Sized against `thread::sleep`'s real granularity rather than
    /// [`POLL_INTERVAL`]'s nominal one: an unmodified Windows timer resolution of
    /// 15.6 ms, plus whatever a busy machine adds on top. 200 ms absorbs more than
    /// ten consecutive late turns, and costs nothing when turns are on time,
    /// because the drain loop empties whatever is there rather than a fixed amount.
    const POLL_BUFFER_HNS: i64 = 200 * 10_000;

    /// See `super::would_capture_pubsplash`. Whether a Desktop Audio source
    /// pinned to `device_id` would capture Pubsplash's own output.
    ///
    /// Endpoint loopback captures *everything* on an endpoint, so pinning the one
    /// Pubsplash plays out of would put its own speech, sound cues and monitoring
    /// back into the stream. The Desktop Audio dialog refuses that pairing, but a
    /// refusal at the dialog is not enough on its own: the output device can be
    /// changed afterwards, and a `None` output setting follows the *system*
    /// default, which moves when a headset is plugged in. So the question is asked
    /// again here, every time the device is opened.
    ///
    /// An unknown effective output device (no default endpoint, or the enumeration
    /// failed) counts as a collision. Capturing all endpoints when we meant one is
    /// a smaller wrong answer than broadcasting our own audio back at the listener.
    pub fn would_capture_pubsplash(device_id: &str) -> bool {
        match device::effective_output_device_id() {
            Some(output) => output == device_id,
            None => true,
        }
    }

    /// Opens a Desktop Audio source, in one of three ways.
    ///
    /// Returns the client and how it must be pumped — see [`Pump`], and note that
    /// only the endpoint-loopback case is polled.
    fn open_desktop_audio(device_id: Option<&str>) -> Result<(AudioClient, bool), String> {
        // Process-exclusion loopback with our own process as the excluded tree: all
        // system audio except Pubsplash itself. This keeps locally played TTS and
        // sound cues out of the capture, so they can never feed back into the
        // stream — and it is the *only* form that can exclude anything, because
        // Windows' process-loopback activation carries no endpoint id at all.
        let all_endpoints = || {
            AudioClient::new_application_loopback_client(std::process::id(), false)
                .map_err(|e| format!("opening the desktop audio loopback client: {e}"))
                .map(|client| (client, false))
        };

        let Some(device_id) = device_id else {
            return all_endpoints();
        };
        if would_capture_pubsplash(device_id) {
            log::warn!(
                "Desktop Audio is pinned to the device Pubsplash itself plays out of, which would \
                 feed its own speech and sound cues back into the stream. Capturing every output \
                 device instead, with Pubsplash excluded. Choose a different output device in \
                 Preferences > Audio, or a different capture device for this source."
            );
            return all_endpoints();
        }
        // Endpoint loopback: everything rendered to this one device. Nothing is
        // excluded, which is exactly why the check above has to have passed.
        device::render_device(device_id)?
            .get_iaudioclient()
            .map_err(|e| format!("activating the output device's audio client: {e}"))
            .map(|client| (client, true))
    }

    /// Opens the device and pumps it until `stop` is set. `started` is called once
    /// audio is actually flowing, which is also what resets the retry backoff.
    pub fn run(
        kind: &CaptureKind,
        producer: &mut Producer<f32>,
        stop: &AtomicBool,
        stats: &CaptureStats,
        started: impl FnOnce(),
    ) -> Result<(), String> {
        let format = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            SAMPLE_RATE as usize,
            CHANNELS,
            None,
        );

        // `endpoint_loopback` decides how the stream is pumped below; see [`Pump`].
        let (mut client, endpoint_loopback) = match kind {
            CaptureKind::Microphone { device_id } => (
                device::capture_device(device_id.as_deref())?
                    .get_iaudioclient()
                    .map_err(|e| format!("activating the microphone's audio client: {e}"))?,
                false,
            ),
            CaptureKind::DesktopAudio { device_id } => open_desktop_audio(device_id.as_deref())?,
            CaptureKind::Application { pid } => (
                AudioClient::new_application_loopback_client(*pid, true)
                    .map_err(|e| format!("opening the loopback client for process {pid}: {e}"))?,
                false,
            ),
        };

        // Endpoint loopback cannot be event-driven (see [`Pump`]), so it is the one
        // form initialized for polling.
        let mode = if endpoint_loopback {
            StreamMode::PollingShared {
                autoconvert: true,
                // Explicitly bigger than the default period, unlike every other
                // path here. A polled reader is woken by `thread::sleep`, whose
                // floor is the system timer resolution — 15.6 ms unless something
                // on the machine has asked for better, and nothing here does. A
                // default-sized buffer is one device period, so a turn that lands
                // late has nowhere to put the audio that arrived meanwhile and
                // WASAPI discards it. This is the headroom that turns that loss
                // into latency the drain loop below pays straight back off.
                buffer_duration_hns: POLL_BUFFER_HNS,
            }
        } else {
            StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 0,
            }
        };
        // What the device itself runs at, before `autoconvert` puts a resampler in
        // the way to give us the 48 kHz stereo float we asked for. Worth a line
        // because that resampler is a suspect whenever a source sounds wrong and
        // nothing else in the log moves — and it is invisible from anywhere else.
        match client.get_mixformat() {
            Ok(mix) => {
                let (rate, channels) = (mix.get_samplespersec(), mix.get_nchannels());
                if rate == SAMPLE_RATE && channels as usize == CHANNELS {
                    log::debug!("Capture device is {rate} Hz, {channels} channels (no conversion)");
                } else {
                    log::info!(
                        "Capture device is {rate} Hz, {channels} channels; Windows is converting it \
                         to {SAMPLE_RATE} Hz, {CHANNELS} channels"
                    );
                }
            }
            // Not knowing the device format costs us a diagnostic, not the capture.
            Err(e) => log::debug!("Could not read the capture device's format: {e}"),
        }
        client
            .initialize_client(&format, &Direction::Capture, &mode)
            .map_err(|e| format!("initializing the capture stream: {e}"))?;

        let pump = if endpoint_loopback {
            Pump::Poll(POLL_INTERVAL)
        } else {
            Pump::Event(
                client
                    .set_get_eventhandle()
                    .map_err(|e| format!("setting up the capture event: {e}"))?,
            )
        };
        let capture = client
            .get_audiocaptureclient()
            .map_err(|e| format!("getting the capture client: {e}"))?;
        let blockalign = format.get_blockalign() as usize;

        client
            .start_stream()
            .map_err(|e| format!("starting the capture stream: {e}"))?;
        started();
        // A reopen is a new device session, and carrying the previous one's drops
        // into it would blame this device for the last one's trouble.
        stats.reset();

        let mut byte_queue: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
        let mut guard = StallGuard::default();
        let mut timeline = DeviceTimeline::new();
        // The first packet after a start carries `DATA_DISCONTINUITY` as a matter of
        // course — there is a gap between the device starting and us reading it, and
        // Windows says so. Counting it would put a 1 in every log line.
        let mut first_packet = true;
        while !stop.load(Ordering::Relaxed) {
            // Drain *every* packet the device has waiting, not just one.
            //
            // `read_from_device_to_deque` is a single `GetBuffer`/`ReleaseBuffer`
            // pair, so it returns exactly one packet — one device period's worth.
            // Reading one per turn is only ever break-even, and on the polled path
            // it is worse than that: `thread::sleep` is bounded below by the system
            // timer resolution, which is 15.6 ms by default, so a nominal 10 ms
            // sleep consumes one 10 ms packet every 15.6 ms. The endpoint's buffer
            // fills, WASAPI drops what will not fit, and the source crackles
            // steadily however quiet the machine is — the loop can never catch up,
            // because falling behind is what each turn *does*.
            //
            // Draining decouples the loop from its own cadence: a late turn costs
            // latency for one turn and is then paid off, rather than compounding.
            // The event path takes the same treatment, where it is a no-op in the
            // ordinary case (the event fires once per packet) and a recovery when
            // a turn runs late.
            loop {
                // The drain's exit is normally "the device has nothing left", but
                // that is not guaranteed to arrive: a device that has a packet ready
                // every time it is asked keeps the loop here indefinitely, and
                // nothing inside it blocks (`push_f32` discards rather than waits,
                // because capture must never block). Stopping is checked each turn
                // so a `SetSources` or a shutdown is not held up by a busy endpoint.
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let new_frames = capture
                    .get_next_packet_size()
                    .map_err(|e| format!("reading the next packet size: {e}"))?
                    .unwrap_or(0);
                if new_frames == 0 {
                    break;
                }
                byte_queue.reserve(new_frames as usize * blockalign);
                let before = byte_queue.len();
                let info = capture
                    .read_from_device_to_deque(&mut byte_queue)
                    .map_err(|e| format!("reading from the device: {e}"))?;
                // `AUDCLNT_BUFFERFLAGS_SILENT` means the contents of that buffer are
                // undefined, not that they are zeros — the crate copies them out
                // regardless, so without this whatever was in that memory would be
                // mixed and broadcast as audio.
                if info.flags.silent {
                    stats.note_silent_packet();
                    silence_appended(&mut byte_queue, before);
                }
                if info.flags.data_discontinuity && !first_packet {
                    stats.note_discontinuity();
                }
                first_packet = false;
                let frames = ((byte_queue.len() - before) / blockalign) as u64;
                stats.add_frames(frames);
                let gap = timeline.observe(info.index, frames, Instant::now());
                if gap > 0 {
                    stats.add_gap_frames(gap);
                }
                stats.set_index_usable(timeline.index_usable());
                if let Some(rate) = timeline.rate_millihz() {
                    stats.set_rate_millihz(rate);
                }
                stats.add_dropped_samples(push_f32(&mut byte_queue, producer) as u64);
            }
            // Timeouts are normal here — loopback with nothing playing times out
            // every turn — so a failed wait is not itself an error. `StallGuard`
            // separates those from a wait that has stopped waiting at all, which
            // would otherwise spin this loop on a core forever; see its docs.
            guard.wait("capture", || pump.wait())?;
        }
        let _ = client.stop_stream();
        Ok(())
    }
}

/// Core Audio capture.
///
/// The three source kinds map onto two different macOS mechanisms, and only the
/// first is built here:
///
/// - a **microphone** is an ordinary input `AudioUnit` on a chosen device, which
///   is what this module is, and needs the `NSMicrophoneUsageDescription`
///   prompt;
/// - **Desktop Audio** is a Core Audio process tap created with
///   `CATapDescription(excludingProcesses:)`, which is very nearly a direct
///   translation of today's default — capture everything except Pubsplash's own
///   output — and is why `would_capture_pubsplash` survives in spirit;
/// - an **Application** source is the same tap mechanism with an *inclusion*
///   list. That is where the process-tree rule has to be rewritten rather than
///   ported: a tap takes a list of process object ids, not a "and its
///   descendants" flag, so `choose_pid`'s walk to the root becomes an explicit
///   enumeration of the tree.
///
/// The tap work needs macOS 14.4 and the audio-capture TCC consent, and none of
/// it works under the App Sandbox. Until it exists those two kinds report a
/// plain failure, which the supervisor above already knows how to show: the
/// source appears in the mixer and says why it is not running, rather than
/// pretending to be live and sending silence.
///
/// **The callback allocates, locks and logs nothing.** It runs on the HAL's
/// real-time thread: it renders into a buffer allocated when the source opened,
/// pushes what it got into the ring, and bumps counters.
#[cfg(target_os = "macos")]
mod imp {
    use super::{CHANNELS, CaptureKind, device, push_samples};
    use crate::audio::coreaudio as ca;
    use crate::audio::tap as ca_tap;
    use crate::audio::health::{CaptureStats, DeviceTimeline};
    use objc2_audio_toolbox::AudioUnitRenderActionFlags;
    use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};
    use rtrb::Producer;
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// What the input callback reaches through.
    ///
    /// The three pointers are the source's own state, parked here for the life
    /// of the unit. They are raw rather than references because this state moves
    /// to the HAL's thread while the unit runs: `run` hands it over before
    /// starting the unit and does not touch it again until the unit is dropped,
    /// which is both the single-producer discipline `rtrb` requires and what
    /// makes the `&mut`s sound. The unit is created after this struct and
    /// dropped before it, and `Unit::drop` stops the callback before returning.
    struct Input {
        /// Filled in after `open_input` returns and before `start`, because the
        /// callback needs the very unit it is being installed on in order to
        /// fetch the audio — and cannot run until `start`.
        unit: AtomicPtr<c_void>,
        producer: *mut Producer<f32>,
        scratch: *mut ca::Scratch,
        timeline: *mut DeviceTimeline,
        stats: *const CaptureStats,
        /// The device's pulse; see [`Self::cycles`]'s use in `run`.
        cycles: AtomicU64,
    }

    /// # Safety
    /// `ref_con` is the `Input` passed to `open_input`, alive for the life of
    /// the unit. `io_data` is null on the input path — the audio is fetched
    /// rather than handed over.
    unsafe extern "C-unwind" fn capture(
        ref_con: NonNull<c_void>,
        flags: NonNull<AudioUnitRenderActionFlags>,
        time: NonNull<AudioTimeStamp>,
        _bus: u32,
        frames: u32,
        _io_data: *mut AudioBufferList,
    ) -> i32 {
        // SAFETY: `Input` outlives the unit, by the contract above.
        let input = unsafe { &*ref_con.as_ptr().cast::<Input>() };
        input.cycles.fetch_add(1, Ordering::Relaxed);

        let unit = input.unit.load(Ordering::Acquire);
        if unit.is_null() {
            return 0;
        }
        // SAFETY: this state belongs to this thread while the unit runs.
        let (scratch, producer, timeline) = unsafe {
            (
                &mut *input.scratch,
                &mut *input.producer,
                &mut *input.timeline,
            )
        };
        let stats = unsafe { &*input.stats };

        // Read before the render, which borrows the scratch for the rest of the
        // cycle. `mSampleTime` is the device's own frame counter and counts
        // *device* frames, so it is restated at our rate before being
        // differenced against a client-rate sample count -- see
        // [`ca::Scratch::client_index`]. It is the same instrumentation Windows
        // reads out of `BufferInfo.index`, so a Mac's health line separates a
        // slipping clock from a dropped buffer the same way.
        //
        // SAFETY: `time` is the HAL's timestamp for this cycle.
        let index = scratch.client_index(unsafe { time.as_ref() }.mSampleTime);

        // SAFETY: `unit` is the live input unit this callback is installed on,
        // and `flags`/`time` are the HAL's for this cycle.
        let samples = match unsafe { scratch.render(unit.cast(), flags, time, frames) } {
            Ok(samples) => samples,
            // Nothing to log from here and nowhere to report it: a failed render
            // is a lost cycle, and a run of them is what the pulse in `run`
            // notices. Counting it as a gap keeps the health line honest.
            Err(_) => {
                stats.add_gap_frames(u64::from(frames));
                return 0;
            }
        };

        let captured = (samples.len() / CHANNELS) as u64;
        stats.add_frames(captured);
        let gap = timeline.observe(index, captured, Instant::now());
        if gap > 0 {
            stats.add_gap_frames(gap);
        }
        stats.set_index_usable(timeline.index_usable());
        if let Some(rate) = timeline.rate_millihz() {
            stats.set_rate_millihz(rate);
        }
        stats.add_dropped_samples(push_samples(samples, producer) as u64);
        0
    }

    /// Why Desktop Audio does not run on macOS, despite being written.
    ///
    /// A Desktop Audio source is a **global tap excluding Pubsplash's own
    /// process**, and that exclusion does not bind reliably. Measured across
    /// about forty runs: a tap built exactly the same way twice captures the
    /// app's own output roughly a quarter of the time, and when it does it does
    /// so for that tap's whole life. Four things were tried and each helped
    /// without fixing it — refusing an empty exclusion list, holding a silent
    /// output stream open so the app is always "playing" when the tap binds,
    /// waiting a dozen render cycles for the audio server to notice that stream,
    /// and verifying a finished tap by playing a probe tone through our own
    /// output and listening for it on the tap.
    ///
    /// It is off rather than best-effort because of **what** leaks. Every spoken
    /// chat message, every cue and every sound Pubsplash plays would go into the
    /// broadcast, silently and for the whole session — the single failure the
    /// Windows half of this file is most carefully built to avoid, and one a
    /// broadcaster would hear only as feedback from their own listeners.
    ///
    /// **Application sources are unaffected and are enabled.** They are an
    /// *inclusion* tap: it captures exactly the processes named and can only
    /// ever hear Pubsplash if the user picks Pubsplash. There is no exclusion,
    /// so there is nothing to bind unreliably.
    const DESKTOP_AUDIO_UNSAFE: &str =
        "capturing desktop audio is not available on macOS yet: macOS cannot yet be relied on \
         to keep Pubsplash's own speech and sounds out of the capture. Capture the app you \
         want with an Application source instead.";

    /// Why this source kind cannot run here, or `None` if it can.
    ///
    /// **Both Desktop Audio shapes are permanently unavailable on macOS**, and
    /// saying so once is the whole point of this function. `open_tap` already
    /// refuses them, but a refusal from *there* arrives as an ordinary failed
    /// attempt: the supervisor backs off and tries again, for ever, and the
    /// mixer strip reads "Desktop Audio (reconnecting)" for the life of the
    /// session. That is a lie about a fixable problem. Asked here instead, the
    /// answer is given once, the thread ends, and the strip says the source is
    /// unavailable and why.
    pub fn unsupported(kind: &CaptureKind) -> Option<String> {
        match kind {
            // The Windows "pin Desktop Audio to one endpoint" form. A screen
            // capture is per-machine rather than per-device, so this is not the
            // same mechanism and is not built -- and it is the *less* safe of
            // the two anyway, being the form that can capture our own output.
            CaptureKind::DesktopAudio { device_id: Some(_) } => Some(
                "capturing one playback device is not supported on macOS; clear the device \
                 and Desktop Audio will capture everything except Pubsplash itself"
                    .to_string(),
            ),
            CaptureKind::DesktopAudio { device_id: None }
            | CaptureKind::Microphone { .. }
            | CaptureKind::Application { .. } => None,
        }
    }

    /// Creates the process tap an Application source needs.
    ///
    /// The two shapes are the two ways `CATapDescription` can be built, and they
    /// line up with the Windows forms almost exactly — see
    /// [`crate::audio::tap`]. What does *not* line up is the process tree: a tap
    /// names processes and has no "and its descendants" flag, so an Application
    /// source enumerates the whole tree here.
    pub fn open_tap(kind: &CaptureKind) -> Result<ca_tap::ProcessTap, String> {
        match kind {
            // Desktop Audio is a ScreenCaptureKit stream, not a tap -- see
            // [`crate::audio::screen_audio`] -- and the pinned form is refused
            // by `unsupported` before any attempt is made.
            CaptureKind::DesktopAudio { .. } => {
                unreachable!("desktop audio does not go through a tap on macOS")
            }
            CaptureKind::Application { pid } => {
                let tree = ca_tap::tree_of(*pid, &device::process_parents());
                let objects: Vec<_> = tree
                    .iter()
                    .filter_map(|pid| ca_tap::process_object_for_pid(*pid))
                    .collect();
                ca_tap::ProcessTap::including(&objects)
            }
            CaptureKind::Microphone { .. } => {
                unreachable!("a microphone does not need a tap")
            }
        }
    }

    /// How long the HAL may go without delivering before the device is declared
    /// gone. [`crate::audio::monitor`]'s reasoning, and the same figure: far
    /// longer than any scheduling hiccup, short enough that the reopen happens
    /// while the user is still wondering why the source went quiet.
    const SILENT_LIMIT: Duration = Duration::from_secs(1);

    /// How often the pulse is checked. Also how promptly a retiring thread
    /// releases the device, which is why it is well under
    /// [`super::WAIT_MS`]'s budget.
    const TICK: Duration = Duration::from_millis(50);

    pub fn run(
        kind: &CaptureKind,
        producer: &mut Producer<f32>,
        stop: &AtomicBool,
        stats: &CaptureStats,
        started: impl FnOnce(),
    ) -> Result<(), String> {
        // Desktop Audio is the one kind that is not an `AudioDeviceID` at all:
        // it is a screen-capture stream, for the reasons its module gives.
        if matches!(kind, CaptureKind::DesktopAudio { .. }) {
            return crate::audio::screen_audio::run(producer, stop, stats, started);
        }
        // A tap, if this source needs one. Held for the whole run: dropping it
        // destroys the aggregate device the unit below is reading from, so it
        // must outlive the unit -- which is why it is bound here and not inside
        // `open_for`.
        let tap;
        let device = match kind {
            CaptureKind::Microphone { device_id } => device::capture_device(device_id.as_deref())?.0,
            other => {
                tap = open_tap(other)?;
                tap.device()
            }
        };
        run_on_device(device, producer, stop, stats, started)
    }

    /// The device half of [`run`], once the source kind has been resolved to an
    /// `AudioDeviceID`. Split out because a microphone and a process tap differ
    /// only in how that id is obtained.
    pub fn run_on_device(
        device: u32,
        producer: &mut Producer<f32>,
        stop: &AtomicBool,
        stats: &CaptureStats,
        started: impl FnOnce(),
    ) -> Result<(), String> {

        // Asked of the device before anything is created, because it decides
        // both how the unit is opened and how `Scratch` is sized, and those two
        // must agree. A device that will not say falls back to stereo, which is
        // what an aggregate built around a tap reports.
        let channels = ca::channels_in(device, ca::Scope::Input).unwrap_or(CHANNELS);
        let device_rate = ca::nominal_rate(device).unwrap_or(f64::from(ca::SAMPLE_RATE));
        // Worth a line for the same reason the Windows half logs its mix format:
        // the converter in the way is a suspect whenever a source sounds wrong
        // and nothing else in the log moves, and it is invisible from anywhere
        // else.
        if channels == CHANNELS && device_rate == f64::from(ca::SAMPLE_RATE) {
            log::debug!("Capture device is {device_rate} Hz, {channels} channels (no conversion)");
        } else {
            log::info!(
                "Capture device is {device_rate} Hz, {channels} channel(s); converting to \
                 {} Hz, {CHANNELS} channels",
                ca::SAMPLE_RATE
            );
        }
        let mut scratch = ca::Scratch::for_device(channels, device_rate);
        let mut timeline = DeviceTimeline::new();
        let input = Input {
            unit: AtomicPtr::new(std::ptr::null_mut()),
            producer: std::ptr::from_mut(producer),
            scratch: std::ptr::from_mut(&mut scratch),
            timeline: std::ptr::from_mut(&mut timeline),
            stats: std::ptr::from_ref(stats),
            cycles: AtomicU64::new(0),
        };
        // SAFETY: `input` is declared first and so outlives `unit`, and
        // `Unit::drop` stops the callback before returning.
        let unit = unsafe {
            ca::open_input(
                device,
                channels,
                Some(capture),
                std::ptr::from_ref(&input) as *mut c_void,
            )?
        };
        // Before `start`, which is the only thing that can make the callback run.
        input.unit.store(unit.raw().cast(), Ordering::Release);
        unit.start()?;
        started();

        let mut seen = 0;
        let mut last_pulse = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(TICK);
            let cycles = input.cycles.load(Ordering::Relaxed);
            if cycles != seen {
                seen = cycles;
                last_pulse = Instant::now();
            } else if last_pulse.elapsed() >= SILENT_LIMIT {
                // Dropped before the `Err`, so the device is released before the
                // supervisor's backoff starts trying to open it again.
                drop(unit);
                return Err("the capture device stopped delivering audio".to_string());
            }
        }
        drop(unit);
        Ok(())
    }

    /// Whether a Desktop Audio source pinned to `device_id` would capture
    /// Pubsplash's own output.
    ///
    /// **Always yes on macOS, and that is an answer rather than a placeholder.**
    /// A pinned Desktop Audio source is the one form `open_tap` refuses, so the
    /// only honest answer for a device id is "this would capture us" — which is
    /// what makes the dialog steer the user to the unpinned form, the one that
    /// excludes Pubsplash by construction rather than by checking.
    pub fn would_capture_pubsplash(_device_id: &str) -> bool {
        true
    }
}

/// Replaces everything appended to `bytes` from `before` onward with zeros.
///
/// Split out so the [`wasapi::BufferFlags::silent`] handling can be tested
/// without a sound card: the crate copies the device buffer into the deque
/// before we ever see the flag, so the fix is to overwrite what it just wrote
/// rather than to skip the read.
fn silence_appended(bytes: &mut std::collections::VecDeque<u8>, before: usize) {
    for byte in bytes.iter_mut().skip(before) {
        *byte = 0;
    }
}

/// Moves whole f32 samples from the byte queue into the ring, dropping
/// samples when the ring is full (mixer stalled or source unattached).
///
/// Returns how many samples were discarded. That number is the difference
/// between a source that is working and one that is crackling, so it is counted
/// rather than thrown away: a full ring means the mixer is not draining this
/// source as fast as the device fills it, and every sample dropped here is a
/// step discontinuity in the middle of a waveform.
fn push_f32(bytes: &mut std::collections::VecDeque<u8>, producer: &mut Producer<f32>) -> usize {
    let available = bytes.len() / 4;
    if available == 0 {
        return 0;
    }
    // Written in one chunk rather than a `push` per sample: at 48 kHz stereo
    // that was 96,000 atomic index stores a second per source, plus four
    // `pop_front`s each.
    let take = producer.slots().min(available);
    if take > 0
        && let Ok(mut chunk) = producer.write_chunk_uninit(take)
    {
        let (first, second) = chunk.as_mut_slices();
        for slot in first.iter_mut().chain(second.iter_mut()) {
            // `available` was computed from the queue length, so each of
            // these four bytes is there.
            slot.write(f32::from_le_bytes([
                bytes.pop_front().unwrap(),
                bytes.pop_front().unwrap(),
                bytes.pop_front().unwrap(),
                bytes.pop_front().unwrap(),
            ]));
        }
        // SAFETY: every slot in both slices was just written.
        unsafe { chunk.commit_all() };
    }
    // Full ring: discard the remainder. Capture must never block.
    let dropped = available - take;
    bytes.drain(..dropped * 4);
    dropped
}

/// Moves `samples` into the ring, dropping what will not fit and reporting how
/// many that was.
///
/// The Core Audio half of [`push_f32`], and the same rule: **capture must never
/// block**, so a full ring is answered by discarding rather than waiting, and
/// the count is what makes that visible in the health line instead of silent.
/// See [`crate::audio::health`] for why that number matters.
///
/// Kept apart from `push_f32` for the reason [`crate::audio::monitor`]'s two
/// ring functions are: this one runs on the HAL's real-time thread, so it takes
/// f32 straight from the device and touches no byte queue.
///
/// Portable and tested here, because a discard rule exercised only by a real
/// sound card is a rule nobody checks.
///
/// Not `#[cfg(target_os = "macos")]`, despite only the macOS `imp` calling it:
/// on Windows its callers are the tests above, and gating it would take them
/// with it — which is the coverage the doc comment is claiming.
#[cfg_attr(windows, allow(dead_code))]
pub fn push_samples(samples: &[f32], producer: &mut Producer<f32>) -> usize {
    if samples.is_empty() {
        return 0;
    }
    // One chunk rather than a `push` per sample; see `push_f32` for the cost.
    let take = producer.slots().min(samples.len());
    if take > 0
        && let Ok(mut chunk) = producer.write_chunk_uninit(take)
    {
        let (first, second) = chunk.as_mut_slices();
        let mut source = samples.iter();
        for slot in first.iter_mut().chain(second.iter_mut()) {
            // `take` is at most `samples.len()`, so the iterator cannot run out.
            slot.write(*source.next().unwrap());
        }
        // SAFETY: every slot in both slices was just written.
        unsafe { chunk.commit_all() };
    }
    samples.len() - take
}

/// Opens an input unit on `device` and pumps it until `stop`.
///
/// The device half of a capture, exposed because [`crate::audio::tap`] uses it
/// to *check* a tap before handing it over — see `exclusion_holds`. The app's
/// own sources always go through [`spawn`].
#[cfg(target_os = "macos")]
pub fn run_on_device(
    device: u32,
    producer: &mut Producer<f32>,
    stop: &AtomicBool,
    stats: &CaptureStats,
    started: impl FnOnce(),
) -> Result<(), String> {
    imp::run_on_device(device, producer, stop, stats, started)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn push_samples_moves_everything_a_ring_will_take() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(8);

        assert_eq!(push_samples(&[1.0, -1.0, 0.5], &mut producer), 0);
        assert_eq!(consumer.slots(), 3);
        assert_eq!(consumer.pop(), Ok(1.0));
    }

    /// A full ring is the mixer not draining this source as fast as the device
    /// fills it. Capture may not block, so the remainder is dropped -- and the
    /// count is the whole point, because it is the difference between a source
    /// that works and one that crackles.
    #[test]
    fn push_samples_drops_what_the_ring_cannot_hold_and_says_how_much() {
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(2);

        let dropped = push_samples(&[1.0, 2.0, 3.0, 4.0, 5.0], &mut producer);

        assert_eq!(dropped, 3);
        assert_eq!(consumer.slots(), 2, "what fitted is still there");
    }

    #[test]
    fn push_samples_into_a_full_ring_drops_all_of_them() {
        let (mut producer, _consumer) = rtrb::RingBuffer::<f32>::new(2);
        producer.push(1.0).unwrap();
        producer.push(2.0).unwrap();

        assert_eq!(push_samples(&[3.0, 4.0], &mut producer), 2);
    }

    #[test]
    fn push_samples_of_nothing_is_nothing() {
        let (mut producer, _consumer) = rtrb::RingBuffer::<f32>::new(4);

        assert_eq!(push_samples(&[], &mut producer), 0);
    }

    /// The ring is circular, so a write that wraps is handed out as two slices.
    /// Filling only the first would corrupt every buffer that straddled the end.
    #[test]
    fn push_samples_fills_a_write_that_wraps() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(4);
        for value in [1.0, 2.0, 3.0] {
            producer.push(value).unwrap();
        }
        for _ in 0..3 {
            consumer.pop().unwrap();
        }

        assert_eq!(push_samples(&[4.0, 5.0, 6.0], &mut producer), 0);

        let got: Vec<f32> = (0..3).map(|_| consumer.pop().unwrap()).collect();
        assert_eq!(got, vec![4.0, 5.0, 6.0]);
    }

    /// Runs the whole microphone path for two seconds and reports what arrived.
    ///
    /// Ignored because it needs a real input device and the macOS microphone
    /// consent prompt. Run it with
    /// `cargo test the_microphone_delivers -- --include-ignored --nocapture`
    /// and talk while it runs.
    ///
    /// `MIC_UID` picks a device other than the default, which is how the three
    /// conversion paths are told apart: a device already at 48 kHz stereo
    /// exercises none of them, a built-in Mac microphone is mono at 48 kHz, and
    /// a Bluetooth headset is mono at 24 kHz and so exercises both. All three
    /// were run when the conversion was written -- and only the third of them
    /// fails if the frame count handed to `AudioUnitRender` is wrong.
    ///
    /// The assertion is only that frames arrived: a peak level cannot be
    /// asserted, because a muted or absent microphone is a legitimate state of
    /// the machine and not a bug in this code. The peak is printed instead, so
    /// the run says whether the audio is real or a flat zero — which is the
    /// difference between a unit that opened and one that is actually wired to
    /// the device.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "needs a microphone and its permission prompt"]
    fn the_microphone_delivers_audio() {
        use crate::audio::mixer::SAMPLE_RATE;

        let rate = SAMPLE_RATE as usize;
        let (producer, mut consumer) = rtrb::RingBuffer::<f32>::new(rate * CHANNELS / 4);
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(CaptureStats::new());
        let (reports, _rx) = crossbeam_channel::unbounded();
        let thread = spawn(
            "Microphone 1".to_string(),
            0,
            CaptureKind::Microphone {
                device_id: std::env::var("MIC_UID").ok(),
            },
            producer,
            Arc::clone(&stop),
            reports,
            Arc::clone(&stats),
        );

        // Drained the way the mixer drains it, so the ring does not simply fill
        // and start dropping -- which would make the `dropped` count below
        // meaningless.
        let mut peak = 0.0f32;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            while let Ok(sample) = consumer.pop() {
                peak = peak.max(sample.abs());
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        stop.store(true, Ordering::Relaxed);
        thread.join().expect("the capture thread should not panic");

        let counters = stats.snapshot();
        println!(
            "frames: {}  dropped: {}  gaps: {}  rate: {} mHz  peak: {peak:.4}",
            counters.frames, counters.dropped_samples, counters.gap_frames, counters.rate_millihz,
        );
        assert!(
            counters.frames > 0,
            "the microphone delivered nothing in two seconds"
        );
        assert_eq!(
            counters.dropped_samples, 0,
            "a ring drained every 10 ms should never overflow"
        );
    }

    /// Unpinned Desktop Audio runs on macOS, so it must not be refused before
    /// it is tried. It is a ScreenCaptureKit stream rather than a tap -- see
    /// [`crate::audio::screen_audio`].
    #[cfg(target_os = "macos")]
    #[test]
    fn desktop_audio_is_available() {
        assert!(
            imp::unsupported(&CaptureKind::DesktopAudio { device_id: None }).is_none(),
            "desktop audio must not be refused up front on macOS"
        );
    }

    /// Pinning Desktop Audio to one device is the form macOS has no mechanism
    /// for, and it must say so rather than retry.
    ///
    /// The reason is not a detail: a source that quietly did nothing would look
    /// like a broken microphone, where this points at the setting to change.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_pinned_desktop_audio_source_says_what_to_do() {
        let refusal = imp::unsupported(&CaptureKind::DesktopAudio {
            device_id: Some("some-device".to_string()),
        })
        .expect("a pinned desktop audio source cannot run on macOS");

        assert!(
            refusal.contains("clear the device"),
            "the refusal has to point somewhere: {refusal}"
        );
    }


    /// Captures one running application and reports what arrived.
    ///
    /// **An Application source is an *inclusion* tap**, which is why it has none
    /// of the exclusion trouble above: it captures exactly the processes named
    /// and can only ever hear Pubsplash if the user picks Pubsplash. The thing
    /// worth checking here is the other half — that naming the root of a process
    /// tree reaches the child that is actually playing, which is what
    /// `tap::tree_of` exists for.
    ///
    /// Ignored, and needs an argument: the executable name of something that is
    /// playing audio right now.
    /// `cargo test application_capture -- --include-ignored --nocapture`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "needs a named app to be playing audio"]
    fn application_capture_reaches_the_process_that_plays() {
        use crate::audio::mixer::SAMPLE_RATE;

        let Ok(name) = std::env::var("PUBSPLASH_TEST_APP") else {
            println!("set PUBSPLASH_TEST_APP to a running app's name to run this");
            return;
        };
        let Some(pid) = device::find_process(&name) else {
            panic!("no running process called {name:?}");
        };
        println!("{name} -> pid {pid}");

        let rate = SAMPLE_RATE as usize;
        let (producer, mut consumer) = rtrb::RingBuffer::<f32>::new(rate * CHANNELS / 4);
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(CaptureStats::new());
        let (reports, rx) = crossbeam_channel::unbounded();
        let thread = spawn(
            "Application 1".to_string(),
            0,
            CaptureKind::Application { pid },
            producer,
            Arc::clone(&stop),
            reports,
            Arc::clone(&stats),
        );

        let mut peak = 0.0f32;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            while let Ok(sample) = consumer.pop() {
                peak = peak.max(sample.abs());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        thread.join().expect("the capture thread should not panic");

        for report in rx.try_iter() {
            println!("state: {:?}", report.state);
        }
        let counters = stats.snapshot();
        println!("frames: {}  peak: {peak:.4}", counters.frames);
        assert!(counters.frames > 0, "the application tap delivered nothing");
    }

    /// The first retry has to be quick — the bug this exists for is a USB
    /// interface a few hundred milliseconds late to enumerate at launch — and
    /// the last one has to be slow enough to wait out a device that is simply
    /// gone without costing anything.
    #[test]
    fn backoff_starts_quick_grows_and_settles() {
        assert_eq!(backoff(0), Duration::from_millis(250));
        let delays: Vec<Duration> = (0..8).map(backoff).collect();
        for pair in delays.windows(2) {
            assert!(pair[1] >= pair[0], "backoff went backwards: {delays:?}");
        }
        assert_eq!(*delays.last().unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn a_wait_that_never_waits_is_eventually_an_error() {
        let mut guard = StallGuard::default();
        for turn in 0..STALL_LIMIT - 1 {
            assert!(
                guard.wait("test", || false).is_ok(),
                "gave up after only {turn} instant failures"
            );
        }
        let last = guard.wait("test", || false);
        assert_eq!(
            last,
            Err("waiting on the test event stopped working".to_string())
        );
    }

    /// The normal case, and the one that must never be mistaken for the case
    /// above: loopback with nothing playing times out on every single turn, for
    /// as long as the source exists.
    #[test]
    fn real_timeouts_are_not_a_stall() {
        let mut guard = StallGuard::default();
        for _ in 0..STALL_LIMIT + 1 {
            let slow_timeout = || {
                std::thread::sleep(IMMEDIATE + Duration::from_millis(2));
                false
            };
            assert!(guard.wait("test", slow_timeout).is_ok());
        }
    }

    /// A signalled wait clears the count, so instant failures scattered among
    /// working turns never add up to a stall.
    #[test]
    fn a_successful_wait_resets_the_count() {
        let mut guard = StallGuard::default();
        for _ in 0..STALL_LIMIT * 3 {
            for _ in 0..STALL_LIMIT - 1 {
                assert!(guard.wait("test", || false).is_ok());
            }
            assert!(guard.wait("test", || true).is_ok());
        }
    }

    #[test]
    fn a_stopped_source_does_not_wait_out_its_backoff() {
        let stop = AtomicBool::new(true);
        let start = std::time::Instant::now();
        sleep_interruptibly(Duration::from_secs(5), &stop);
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    /// The whole point of the supervisor: a device that will not open must not
    /// kill the source. It reports the failure once, keeps retrying in the
    /// background, and still shuts down promptly when the source is retired.
    /// A device id that cannot exist stands in for the real case (a USB
    /// interface the OS has not finished bringing up).
    ///
    /// What is asserted is the *supervisor's* contract, which is portable: one
    /// report carrying the source's name and spawn epoch, a thread still alive
    /// afterwards, and a prompt exit when the source is retired. Only the
    /// wording of the message belongs to the platform, so only that assertion
    /// is `cfg`'d — and on a platform whose capture is not built yet, an
    /// immediate failure is exactly what this test wants to see handled.
    #[test]
    fn a_device_that_will_not_open_is_reported_once_and_retried() {
        let (producer, _consumer) = rtrb::RingBuffer::<f32>::new(64);
        let (tx, rx) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn(
            "Microphone".to_string(),
            7,
            CaptureKind::Microphone {
                device_id: Some("no-such-endpoint".to_string()),
            },
            producer,
            stop.clone(),
            tx,
            Arc::new(CaptureStats::new()),
        );

        let report = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a failure report");
        assert_eq!(report.name, "Microphone");
        assert_eq!(report.epoch, 7, "the spawn epoch is carried through");
        let CaptureState::Failed(message) = report.state else {
            panic!("expected a failure, got {:?}", report.state);
        };
        assert!(
            !message.trim().is_empty(),
            "a failure report must carry a reason"
        );
        #[cfg(windows)]
        assert!(
            message.contains("looking up the configured microphone"),
            "the message should name the step that failed: {message}"
        );

        // Repeats are the caller's to filter, but the thread must still be
        // alive and trying rather than gone.
        assert!(!handle.is_finished());
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !handle.is_finished(),
            "the source gave up instead of retrying"
        );

        stop.store(true, Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(handle.is_finished(), "retiring the source did not stop it");
        handle.join().expect("capture thread panicked");
    }

    /// A full ring must report what it threw away. Silence here is what made a
    /// crackling microphone impossible to diagnose from a log file.
    #[test]
    fn a_full_ring_reports_the_samples_it_discarded() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(4);
        let mut bytes: std::collections::VecDeque<u8> =
            (0..10).flat_map(|n| (n as f32).to_le_bytes()).collect();

        // Four fit; the other six go over the side and are counted.
        assert_eq!(push_f32(&mut bytes, &mut producer), 6);
        assert!(bytes.is_empty(), "the queue is drained either way");
        assert_eq!(consumer.slots(), 4);

        // A ring with room for everything drops nothing.
        while consumer.pop().is_ok() {}
        let mut bytes: std::collections::VecDeque<u8> =
            (0..3).flat_map(|n| (n as f32).to_le_bytes()).collect();
        assert_eq!(push_f32(&mut bytes, &mut producer), 0);
    }

    #[test]
    fn an_empty_queue_drops_nothing() {
        let (mut producer, _consumer) = rtrb::RingBuffer::<f32>::new(4);
        let mut bytes = std::collections::VecDeque::new();
        assert_eq!(push_f32(&mut bytes, &mut producer), 0);
    }

    /// WASAPI's SILENT flag means the buffer contents are *undefined*, not that
    /// they are zeros, and the crate copies them into the queue before we get to
    /// see the flag. Without this the garbage would be mixed and broadcast.
    #[test]
    fn a_silent_packet_is_zeroed_without_disturbing_what_came_before() {
        let mut bytes: std::collections::VecDeque<u8> = VecDeque::from(vec![1, 2, 3, 4]);
        let before = bytes.len();
        bytes.extend([0xDE, 0xAD, 0xBE, 0xEF]);
        silence_appended(&mut bytes, before);
        assert_eq!(
            bytes.iter().copied().collect::<Vec<u8>>(),
            vec![1, 2, 3, 4, 0, 0, 0, 0],
            "only the new bytes are silenced"
        );
    }

    /// The deque wraps once it has been pushed and popped enough, so the
    /// silencing must go through the iterator rather than a contiguous slice.
    #[test]
    fn silencing_works_on_a_wrapped_deque() {
        let mut bytes: VecDeque<u8> = VecDeque::with_capacity(8);
        for byte in 0..8u8 {
            bytes.push_back(byte);
        }
        for _ in 0..6 {
            bytes.pop_front();
        }
        let before = bytes.len();
        bytes.extend([9, 9, 9, 9, 9, 9]);
        silence_appended(&mut bytes, before);
        assert_eq!(
            bytes.iter().copied().collect::<Vec<u8>>(),
            vec![6, 7, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn silencing_an_empty_append_changes_nothing() {
        let mut bytes: VecDeque<u8> = VecDeque::from(vec![1, 2, 3]);
        silence_appended(&mut bytes, 3);
        assert_eq!(bytes.iter().copied().collect::<Vec<u8>>(), vec![1, 2, 3]);
    }

    #[test]
    fn an_uninterrupted_backoff_sleeps_its_full_span() {
        let stop = AtomicBool::new(false);
        let start = std::time::Instant::now();
        sleep_interruptibly(Duration::from_millis(150), &stop);
        assert!(start.elapsed() >= Duration::from_millis(150));
    }
}

