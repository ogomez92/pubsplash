//! The monitoring output thread: plays whatever the mixer taps for monitoring
//! out of the playback device chosen in Preferences (the system default until
//! the user picks one) — see [`crate::audio::render::output_render_device`],
//! which owns that setting and is also what local sound cues open. A change
//! reaches this thread by way of `EngineCommand::ReopenMonitor`, which drops
//! the thread so `engine_loop`'s per-block check spawns a fresh one.
//!
//! This is the mirror image of [`crate::audio::capture`] — same supervisor
//! shape, same backoff, same cooperative stop flag — but playing audio out
//! instead of reading it in. It is opened only while at least one mixer strip is
//! being monitored.
//!
//! The engine and the device are on independent clocks: the mixer is paced by
//! `sleep` on a 10 ms grid, the device by its own. The ring buffer between them
//! absorbs that difference. The engine drops samples when the ring is full and
//! this thread writes silence when it runs dry — neither side ever blocks the
//! other.
//!
//! **Which side of the ring drives is where the two platforms differ**, and it
//! decides the shape of everything below. On Windows the app owns the loop: a
//! WASAPI render client, woken by an event, asking how much room there is and
//! filling it. On macOS the HAL owns the thread and calls *us*, so there is no
//! loop to write — only a callback, and a supervising thread whose whole job is
//! to notice when the callback stops being called. Hence two ring-emptying
//! functions rather than one, [`fill`] and [`pull`], which are the same idea
//! written for a thread the app owns and for a real-time thread it does not.

use crate::audio::device;
use rtrb::Consumer;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long to wait before the next attempt to open the playback device, after
/// `attempt` consecutive failures. Same shape as the capture backoff: quick at
/// first (a device that is a moment late to appear), then slow enough that a
/// device which is simply gone costs nothing to keep waiting for.
fn backoff(attempt: u32) -> Duration {
    const SCHEDULE_MS: [u64; 5] = [250, 500, 1_000, 2_000, 5_000];
    let index = (attempt as usize).min(SCHEDULE_MS.len() - 1);
    Duration::from_millis(SCHEDULE_MS[index])
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

/// Spawns the monitoring output thread. It runs until `stop` is set, reopening
/// the device with a backoff whenever it fails, so unplugging the headphones
/// mid-session does not silently end monitoring.
///
/// Failures are logged only: monitoring is a convenience, and nothing about it
/// may disturb the stream.
pub fn spawn(consumer: Consumer<f32>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("audio-monitor".into())
        .spawn(move || {
            device::ensure_com_initialized();
            let mut consumer = consumer;
            let mut failures: u32 = 0;
            while !stop.load(Ordering::Relaxed) {
                match run(&mut consumer, &stop) {
                    Ok(()) => return,
                    Err(e) => {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        // Only the first failure of a run is news; the retries
                        // after it say the same thing every few seconds.
                        if failures == 0 {
                            log::error!("Monitoring output failed: {e}; retrying");
                        } else {
                            log::debug!("Monitoring output still failing: {e}");
                        }
                        sleep_interruptibly(backoff(failures), &stop);
                        failures = failures.saturating_add(1);
                    }
                }
            }
        })
        .expect("spawning monitor thread")
}

/// Opens Pubsplash's chosen playback device and feeds it until `stop` is set.
///
/// The platform seam of the monitoring path. The supervisor above it — the
/// backoff, the stop flag, the "only the first failure is news" logging — is
/// portable and stays where it is.
fn run(consumer: &mut Consumer<f32>, stop: &AtomicBool) -> Result<(), String> {
    imp::run(consumer, stop)
}

#[cfg(windows)]
mod imp {
    use super::fill;
    use crate::audio::mixer::{CHANNELS, SAMPLE_RATE};
    use rtrb::Consumer;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use wasapi::{Direction, SampleType, StreamMode, WaveFormat};

    pub fn run(consumer: &mut Consumer<f32>, stop: &AtomicBool) -> Result<(), String> {
        let format = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            SAMPLE_RATE as usize,
            CHANNELS,
            None,
        );

        let mut client = crate::audio::render::output_render_device()?
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
        log::debug!("Monitoring output is running");

        let mut bytes: VecDeque<u8> = VecDeque::new();
        let mut guard = crate::audio::capture::StallGuard::default();
        while !stop.load(Ordering::Relaxed) {
            let frames = client
                .get_available_space_in_frames()
                .map_err(|e| format!("reading the available playback space: {e}"))?
                as usize;
            if frames > 0 {
                bytes.clear();
                bytes.reserve(frames * blockalign);
                fill(&mut bytes, consumer, frames * CHANNELS);
                render
                    .write_to_device_from_deque(frames, &mut bytes, None)
                    .map_err(|e| format!("writing to the playback device: {e}"))?;
            }
            // A timeout is normal when the device wants nothing yet, so the result
            // is not an error on its own. It used to be discarded outright, which
            // also discarded the case where the wait fails instantly and this loop
            // spins; `StallGuard` tells the two apart.
            guard.wait("playback", || {
                event.wait_for_event(crate::audio::capture::WAIT_MS).is_ok()
            })?;
        }
        let _ = client.stop_stream();
        Ok(())
    }
}

/// Core Audio monitoring output: the same HAL output unit
/// [`crate::audio::render`] opens for a cue, but its callback pulls the ring
/// instead of walking a fixed slice.
///
/// **The callback is [`pull`] and nothing else.** It runs on the HAL's
/// real-time thread, so it does not allocate, lock or log — which is why it
/// cannot be `fill`, despite the two being the same idea: `fill` builds a
/// `VecDeque<u8>` of little-endian bytes, and both the queue and the byte
/// conversion are work this side must not do. `rtrb`'s read chunk is wait-free
/// and copies straight into the HAL's own buffer.
///
/// The other difference from Windows is how a dead device is noticed. There the
/// app owns the loop and [`crate::audio::capture::StallGuard`] watches a wait
/// that stops waiting; here the HAL owns the thread, and a device that goes away
/// simply stops calling us — a silence indistinguishable, from inside the
/// callback, from a device that is merely idle. So the callback counts its
/// cycles and the supervising thread watches that counter instead. That is what
/// keeps this module's promise that unplugging the headphones mid-session
/// reopens rather than silently ending monitoring.
#[cfg(target_os = "macos")]
mod imp {
    use super::pull;
    use crate::audio::coreaudio as ca;
    use objc2_audio_toolbox::AudioUnitRenderActionFlags;
    use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};
    use rtrb::Consumer;
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// What the render callback reads.
    ///
    /// `consumer` is a raw pointer rather than a reference because the ring's
    /// consuming end moves to the HAL's thread for the life of the unit: `run`
    /// hands it over before starting the unit and does not touch it again until
    /// the unit is dropped, which is the single-consumer discipline `rtrb`
    /// requires. The unit is created after this struct and dropped before it,
    /// and `Unit::drop` stops the callback before returning, so the pointer
    /// cannot outlive what it points at.
    struct Tap {
        consumer: *mut Consumer<f32>,
        /// Bumped once per cycle, and read only by [`run`]. The device's own
        /// pulse: see the module note above for what its stopping means.
        cycles: AtomicU64,
    }

    /// # Safety
    /// `ref_con` is the `Tap` passed to `open_output`, alive for the life of the
    /// unit; `io_data` is the HAL's buffer list for this cycle.
    unsafe extern "C-unwind" fn render(
        ref_con: NonNull<c_void>,
        _flags: NonNull<AudioUnitRenderActionFlags>,
        _time: NonNull<AudioTimeStamp>,
        _bus: u32,
        frames: u32,
        io_data: *mut AudioBufferList,
    ) -> i32 {
        // SAFETY: `Tap` outlives the unit, by the contract above.
        let tap = unsafe { &*ref_con.as_ptr().cast::<Tap>() };
        // Before the early return, so a device that asks for buffers we cannot
        // read still counts as alive and is not reopened round and round.
        tap.cycles.fetch_add(1, Ordering::Relaxed);

        let Some(out) = (unsafe { ca::hal_buffer(io_data, frames) }) else {
            return 0;
        };
        // SAFETY: the ring's consuming end belongs to this thread while the unit
        // runs, and no other thread touches it in that window.
        let consumer = unsafe { &mut *tap.consumer };
        let filled = pull(consumer, out);
        // An underrun is a gap in the monitor, never a stall in the mixer. The
        // HAL does not zero the buffer for us.
        out[filled..].fill(0.0);
        0
    }

    /// How long the HAL may go without asking for audio before the device is
    /// declared gone. A second, for [`crate::audio::capture::STALL_LIMIT`]'s
    /// reasoning: far longer than any scheduling hiccup at a device period of a
    /// few milliseconds, short enough that the reopen happens while the user is
    /// still wondering why the monitor went quiet.
    const SILENT_LIMIT: Duration = Duration::from_secs(1);

    /// How often the pulse is checked. Also how promptly a retiring thread
    /// releases the device, so it is kept well under
    /// [`crate::audio::capture::WAIT_MS`]'s equivalent budget.
    const TICK: Duration = Duration::from_millis(50);

    pub fn run(consumer: &mut Consumer<f32>, stop: &AtomicBool) -> Result<(), String> {
        let device = crate::audio::render::output_render_device()?;
        let tap = Tap {
            consumer: std::ptr::from_mut(consumer),
            cycles: AtomicU64::new(0),
        };
        // SAFETY: `tap` is declared first and so outlives `unit`, and
        // `Unit::drop` stops the callback before returning.
        let unit = unsafe {
            ca::open_output(
                device,
                Some(render),
                std::ptr::from_ref(&tap) as *mut c_void,
            )?
        };
        unit.start()?;
        log::debug!("Monitoring output is running");

        let mut seen = 0;
        let mut last_pulse = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(TICK);
            let cycles = tap.cycles.load(Ordering::Relaxed);
            if cycles != seen {
                seen = cycles;
                last_pulse = Instant::now();
            } else if last_pulse.elapsed() >= SILENT_LIMIT {
                // Dropped before the `Err`, so the device is released before the
                // supervisor's backoff starts trying to open it again.
                drop(unit);
                return Err("the playback device stopped asking for audio".to_string());
            }
        }
        drop(unit);
        Ok(())
    }
}

/// Appends `samples` samples' worth of little-endian f32 bytes to `bytes`,
/// taking what the ring has and padding the rest with silence. An underrun is
/// a gap in the monitor, never a stall in the mixer.
///
/// The Windows half of the pair; [`pull`] is the Core Audio one. Allowed to be
/// unreached off Windows rather than `cfg`'d out, so that its tests — which need
/// no sound card and pin behaviour both platforms depend on — keep running
/// everywhere. Scoped to this one function: everything else in the file is now
/// live on both platforms.
#[cfg_attr(not(windows), allow(dead_code))]
fn fill(bytes: &mut VecDeque<u8>, consumer: &mut Consumer<f32>, samples: usize) {
    // Read in one chunk rather than a `pop` per sample; see
    // `mixer::pull_block` for why that adds up.
    let take = consumer.slots().min(samples);
    let mut filled = 0;
    if take > 0
        && let Ok(chunk) = consumer.read_chunk(take)
    {
        let (first, second) = chunk.as_slices();
        for &sample in first.iter().chain(second.iter()) {
            bytes.extend(sample.to_le_bytes());
        }
        filled = first.len() + second.len();
        chunk.commit_all();
    }
    for _ in filled..samples {
        bytes.extend(0f32.to_le_bytes());
    }
}

/// Moves as much of the ring as `out` will hold into it, and reports how many
/// samples that was. The caller silences the rest.
///
/// The Core Audio half of [`fill`], kept apart from it for the reason the macOS
/// `imp` above gives: this one runs on a real-time thread and so writes f32
/// straight into the device's buffer, where `fill` may build a byte queue
/// because the Windows loop is the app's own thread.
///
/// Portable, and tested here, because a ring-emptying rule that is only exercised
/// by a real sound card is a rule nobody checks.
///
/// Not `#[cfg(target_os = "macos")]`, despite only the macOS `imp` calling it:
/// on Windows its callers are the tests above, and gating it would take them
/// with it — which is the coverage the doc comment is claiming.
#[cfg_attr(windows, allow(dead_code))]
fn pull(consumer: &mut Consumer<f32>, out: &mut [f32]) -> usize {
    // One chunk rather than a `pop` per sample; see `mixer::pull_block` for why
    // that adds up.
    let take = consumer.slots().min(out.len());
    if take == 0 {
        return 0;
    }
    let Ok(chunk) = consumer.read_chunk(take) else {
        return 0;
    };
    // The ring is circular, so what looks like one run of samples can be two.
    let filled = {
        let (first, second) = chunk.as_slices();
        out[..first.len()].copy_from_slice(first);
        out[first.len()..first.len() + second.len()].copy_from_slice(second);
        first.len() + second.len()
    };
    chunk.commit_all();
    filled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_ring_renders_silence() {
        let (_producer, mut consumer) = rtrb::RingBuffer::<f32>::new(8);
        let mut bytes = VecDeque::new();
        fill(&mut bytes, &mut consumer, 4);
        assert_eq!(bytes.len(), 16, "four f32 samples of silence");
        assert!(bytes.iter().all(|&b| b == 0));
    }

    #[test]
    fn a_partly_filled_ring_is_padded_with_silence() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(8);
        producer.push(1.0).unwrap();
        producer.push(-1.0).unwrap();
        let mut bytes = VecDeque::new();
        fill(&mut bytes, &mut consumer, 4);
        let out: Vec<u8> = bytes.into_iter().collect();
        let decoded: Vec<f32> = out
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(decoded, vec![1.0, -1.0, 0.0, 0.0]);
    }

    #[test]
    fn pull_takes_what_the_ring_has_and_reports_how_much() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(8);
        producer.push(1.0).unwrap();
        producer.push(-1.0).unwrap();
        let mut out = [7.0; 4];

        let filled = pull(&mut consumer, &mut out);

        assert_eq!(filled, 2);
        assert_eq!(out[..2], [1.0, -1.0]);
        // The rest is the caller's to silence, and must be left alone here --
        // the callback fills exactly `out[filled..]`, so writing it twice would
        // be the one place these two could disagree.
        assert_eq!(out[2..], [7.0, 7.0]);
    }

    #[test]
    fn pull_from_an_empty_ring_takes_nothing() {
        let (_producer, mut consumer) = rtrb::RingBuffer::<f32>::new(8);
        let mut out = [7.0; 4];

        assert_eq!(pull(&mut consumer, &mut out), 0);
        assert_eq!(out, [7.0; 4]);
    }

    /// The ring is circular, so a read that wraps comes back as two slices. A
    /// callback that copied only the first would drop audio every time the write
    /// head passed the end -- a periodic tick nobody would trace back to here.
    #[test]
    fn pull_reassembles_a_read_that_wraps() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(4);
        // Fill and drain most of it, to move both heads near the end.
        for value in [1.0, 2.0, 3.0] {
            producer.push(value).unwrap();
        }
        let mut out = [0.0; 3];
        pull(&mut consumer, &mut out);
        // Now write across the wrap.
        for value in [4.0, 5.0, 6.0] {
            producer.push(value).unwrap();
        }

        let mut out = [0.0; 3];
        let filled = pull(&mut consumer, &mut out);

        assert_eq!(filled, 3);
        assert_eq!(out, [4.0, 5.0, 6.0]);
    }

    /// More in the ring than the device asked for: take exactly a bufferful and
    /// leave the rest, rather than overrunning or dropping it.
    #[test]
    fn pull_never_takes_more_than_the_buffer_holds() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(8);
        for value in [1.0, 2.0, 3.0, 4.0] {
            producer.push(value).unwrap();
        }
        let mut out = [0.0; 2];

        assert_eq!(pull(&mut consumer, &mut out), 2);
        assert_eq!(out, [1.0, 2.0]);
        assert_eq!(consumer.slots(), 2, "the rest stays for the next cycle");
    }

    /// Drives the whole monitoring path for a second: the supervisor thread, the
    /// device open, the HAL callback and [`pull`], fed the way the mixer feeds
    /// it — 10 ms at a time, in real time.
    ///
    /// Ignored because it makes a noise, and like the cue tone in
    /// [`crate::audio::render`] there is nothing to assert: whether the callback
    /// is wired up right is a question about pitch and dropouts that only an ear
    /// answers. Run it with
    /// `cargo test the_monitor_thread_plays -- --include-ignored`.
    ///
    /// A clean tone means the ring, the callback and the silence-padding agree.
    /// A steady tick means [`pull`] is losing the wrapped half of a read. Silence
    /// means the device never opened — the log says why.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "plays audio"]
    fn the_monitor_thread_plays_what_the_ring_carries() {
        use crate::audio::mixer::{CHANNELS, SAMPLE_RATE};

        // A quarter of a second of slack, so a late feed is absorbed rather than
        // heard. The same reason the engine's own ring is generous.
        let rate = SAMPLE_RATE as usize;
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(rate * CHANNELS / 4);
        let stop = Arc::new(AtomicBool::new(false));
        let monitor = spawn(consumer, Arc::clone(&stop));

        let block_frames = rate / 100;
        let mut frame = 0usize;
        for _ in 0..100 {
            for _ in 0..block_frames {
                let t = frame as f32 / rate as f32;
                let value = (t * 440.0 * std::f32::consts::TAU).sin() * 0.2;
                // A full ring is the mixer's normal answer to a device that is a
                // little slow; dropping is what the engine does too.
                let _ = producer.push(value);
                let _ = producer.push(value);
                frame += 1;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        stop.store(true, Ordering::Relaxed);
        monitor.join().expect("the monitor thread should not panic");
    }

    #[test]
    fn backoff_starts_quick_grows_and_settles() {
        assert_eq!(backoff(0), Duration::from_millis(250));
        assert_eq!(backoff(99), Duration::from_secs(5));
    }
}
