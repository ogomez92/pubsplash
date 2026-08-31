//! Desktop Audio on macOS: everything the machine is playing, except us.
//!
//! **This is the second answer to the problem, and the first one is why the
//! second exists.** A Core Audio process tap built with
//! `CATapDescription(excludingProcesses:)` maps onto Windows' Desktop Audio
//! almost exactly on paper, and it is what `audio/tap.rs` was written for. Its
//! exclusion does not bind reliably. Measured on one machine in one sitting,
//! playing a 440 Hz tone at 0.8 out of Pubsplash's own output and listening for
//! it on the capture: **seven of fourteen taps heard it at 0.50 to 0.74** — and
//! every one of those taps had already passed the tap module's own verification,
//! so the check could not be trusted either. What leaks is every spoken chat
//! message and every cue, silently, for a whole broadcast.
//!
//! ScreenCaptureKit's [`SCStreamConfiguration::setExcludesCurrentProcessAudio`]
//! is Apple's supported flag for the same idea, and on the same machine, the
//! same day, the same probe: **twelve of twelve runs heard exactly 0.0000**,
//! while another process's audio was captured in every one. That gap is why
//! Desktop Audio is a screen-capture stream here and not a process tap.
//! [`crate::audio::tap`] keeps the tap for **Application** sources, which are an
//! *inclusion* tap and have never had the problem: they capture exactly the
//! processes named, so they can only hear Pubsplash if the user picks
//! Pubsplash.
//!
//! Three things about this shape are worth keeping in mind.
//!
//! **It costs the Screen Recording permission**, which is the most intrusive
//! prompt the app asks for, and it is asked for even though no video is ever
//! looked at — a stream must have a content filter and a size, so it captures a
//! 2x2 pixel region of a display once a second and throws it away. There is no
//! audio-only `SCStream`. A refusal is reported like any other capture failure.
//!
//! **The samples arrive on a dispatch queue, not the HAL's render thread.** That
//! is a real relaxation of `audio/capture.rs`'s rules — this is not a real-time
//! thread and a lock here cannot cause an audible priority inversion — but the
//! queue is still serviced in step with the audio, so the work per buffer is
//! kept to the interleave and the ring push, and the one buffer it needs is
//! allocated once and reused.
//!
//! **ScreenCaptureKit hands over non-interleaved audio**, one `AudioBuffer` per
//! channel, where the mixer wants frames. [`interleave`] is that, and it is the
//! one piece here that is testable without a display.

use crate::audio::capture::push_samples;
use crate::audio::health::CaptureStats;
use crate::audio::mixer::{CHANNELS, SAMPLE_RATE};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_audio_types::AudioBufferList;
use objc2_core_media::{CMBlockBuffer, CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType, SCWindow,
};
use rtrb::Producer;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long the stream may go without delivering before it is declared dead.
///
/// [`crate::audio::monitor`]'s reasoning and the same figure: far longer than
/// any scheduling hiccup, short enough that the reopen happens while the user is
/// still wondering why the source went quiet. A screen-capture stream delivers
/// audio continuously whether or not anything is making a sound, so silence here
/// really does mean the stream has stopped.
const SILENT_LIMIT: Duration = Duration::from_secs(1);

/// How often the pulse is checked, and so how promptly a retiring source lets
/// the stream go.
const TICK: Duration = Duration::from_millis(50);

/// How long to wait for the permission prompt to be answered.
///
/// The first call to [`SCShareableContent`] is what raises it, and it does not
/// come back until the user has decided. Generous, because deciding involves
/// opening System Settings the first time.
const CONTENT_TIMEOUT: Duration = Duration::from_secs(60);

/// The video the stream is obliged to carry. Two pixels, once a second.
///
/// There is no audio-only `SCStream`: a stream must have a content filter and a
/// frame size. This is the smallest, slowest thing it will accept, and no frame
/// is ever collected — only [`SCStreamOutputType::Audio`] is subscribed to.
const VIDEO_EDGE: usize = 2;

/// What the delegate needs to reach from the dispatch queue.
///
/// The producer is behind a `Mutex` rather than being moved onto the queue
/// because the supervisor thread has to be able to take it back when the source
/// retires. It is uncontended: a serial queue is the only thing that locks it
/// while the stream is running.
pub struct Ivars {
    inner: Mutex<Sink>,
    /// Bumped once per delivered buffer, so the supervisor can tell a live
    /// stream from a dead one without touching the lock.
    cycles: AtomicU64,
}

/// The delegate's own state, all of it behind one lock.
struct Sink {
    producer: Option<Producer<f32>>,
    /// The interleaved frame buffer handed to the ring. Grown to fit and then
    /// reused, so a steady stream stops allocating after its first buffer.
    frames: Vec<f32>,
    /// Storage for the `AudioBufferList` the sample buffer is read into. `u64`
    /// rather than `u8` so it is aligned for the pointer the list holds.
    list: Vec<u64>,
    dropped: u64,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "PubsplashScreenAudio"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        // Named for the Objective-C selector it implements, which is what the
        // protocol requires; the lint is about Rust naming and does not apply.
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Audio {
                return;
            }
            self.ivars().cycles.fetch_add(1, Ordering::Relaxed);
            let mut sink = match self.ivars().inner.lock() {
                Ok(sink) => sink,
                // A poisoned lock means a previous buffer panicked. Keep the
                // stream alive rather than compounding it; `capture` recovers
                // poisoning everywhere else for the same reason.
                Err(e) => e.into_inner(),
            };
            // SAFETY: the sample buffer is the system's, live for this call.
            unsafe { sink.take(sample_buffer) };
        }
    }
);

impl Sink {
    /// Reads one sample buffer into the ring.
    ///
    /// # Safety
    /// `sample_buffer` must be the live buffer the delegate was handed.
    unsafe fn take(&mut self, sample_buffer: &CMSampleBuffer) {
        // How big the list has to be is asked rather than guessed: a guess
        // answers `kCMSampleBufferError_ArrayTooSmall` (-12737) and nothing else
        // says why.
        let mut needed = 0usize;
        // SAFETY: a size query, with no output list to write into.
        let status = unsafe {
            sample_buffer.audio_buffer_list_with_retained_block_buffer(
                &mut needed,
                std::ptr::null_mut(),
                0,
                None,
                None,
                0,
                std::ptr::null_mut(),
            )
        };
        if status != 0 || needed == 0 {
            return;
        }
        let words = needed.div_ceil(std::mem::size_of::<u64>()).max(1);
        if self.list.len() < words {
            self.list.resize(words, 0);
        }
        let list = self.list.as_mut_ptr().cast::<AudioBufferList>();
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        // SAFETY: `list` points at `words * 8 >= needed` aligned bytes.
        let status = unsafe {
            sample_buffer.audio_buffer_list_with_retained_block_buffer(
                std::ptr::null_mut(),
                list,
                needed,
                None,
                None,
                0,
                &mut block,
            )
        };
        // The block buffer comes back **retained**, as the name says, and owning
        // it here is what returns it: without this every buffer leaks its audio.
        let _block = std::ptr::NonNull::new(block).map(|block| {
            // SAFETY: a +1 reference this scope now owns.
            unsafe { objc2_core_foundation::CFRetained::from_raw(block) }
        });
        if status != 0 {
            return;
        }
        // SAFETY: the call above filled it.
        let list = unsafe { &*list };
        // SAFETY: `mNumberBuffers` is the length of the array that follows.
        let buffers = unsafe {
            std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize)
        };
        let planes: Vec<&[f32]> = buffers
            .iter()
            .filter_map(|buffer| {
                let count = buffer.mDataByteSize as usize / std::mem::size_of::<f32>();
                if buffer.mData.is_null() || count == 0 {
                    return None;
                }
                // SAFETY: the list says this buffer holds `count` floats.
                Some(unsafe { std::slice::from_raw_parts(buffer.mData.cast::<f32>(), count) })
            })
            .collect();
        interleave(&planes, &mut self.frames);
        if let Some(producer) = self.producer.as_mut() {
            self.dropped += push_samples(&self.frames, producer) as u64;
        }
    }
}

/// The amplitude of one frequency in `samples`, by the Goertzel algorithm.
///
/// **Peak level cannot answer "is this our audio?", and the test below needs
/// that answer.** A desktop capture hears whatever the machine is playing, so a
/// loud window means nothing on its own — music, a video call or a notification
/// all read the same as a leak. Measuring the energy at the one frequency the
/// probe tone is at ignores all of it: the test asks not "is it loud" but "is
/// *our* tone in there".
///
/// Goertzel rather than a whole FFT because one bin is all that is wanted, and
/// it is phase-independent, which matters because nothing lines the capture up
/// with the playback.
#[cfg_attr(not(test), allow(dead_code))]
fn tone_amplitude(samples: &[f32], hz: f32, rate: f32) -> f32 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let k = (n as f32 * hz / rate).round();
    let w = std::f32::consts::TAU * k / n as f32;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    2.0 * power.max(0.0).sqrt() / n as f32
}

/// Lays non-interleaved planes out as interleaved stereo frames.
///
/// ScreenCaptureKit delivers one `AudioBuffer` per channel; the mixer wants
/// `L R L R`. Reading planar audio as interleaved is not a crash — it is speech
/// at half speed in one ear — so this is the piece worth testing, and it is the
/// only one here that can be tested without a display.
///
/// A mono stream is copied to both ears, and anything wider than stereo is taken
/// as its first pair, for the same reasons as
/// [`crate::audio::coreaudio::Scratch`]'s conversion.
fn interleave(planes: &[&[f32]], out: &mut Vec<f32>) {
    out.clear();
    let Some(first) = planes.first() else {
        return;
    };
    let frames = planes.iter().map(|p| p.len()).min().unwrap_or(0);
    out.reserve(frames * CHANNELS);
    match planes.len() {
        0 => {}
        1 => {
            for &sample in &first[..frames] {
                out.push(sample);
                out.push(sample);
            }
        }
        _ => {
            let (left, right) = (planes[0], planes[1]);
            for frame in 0..frames {
                out.push(left[frame]);
                out.push(right[frame]);
            }
        }
    }
}

/// Captures everything the machine is playing, except this process, until
/// `stop`.
///
/// Shaped like [`crate::audio::capture`]'s `run_on_device`: it reports through
/// `started` once audio is actually arriving, and returns `Err` when the stream
/// stops delivering so the supervisor's backoff can reopen it.
pub fn run(
    producer: &mut Producer<f32>,
    stop: &AtomicBool,
    stats: &CaptureStats,
    started: impl FnOnce(),
) -> Result<(), String> {
    let display_filter = filter()?;
    let config = configuration();

    // The producer is lent to the delegate and taken back before returning, so
    // the caller's `&mut` is never aliased while the stream is running.
    let output = Output::new(std::mem::replace(producer, empty_producer()));
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &display_filter,
            &config,
            None,
        )
    };
    let queue = DispatchQueue::new("pubsplash-desktop-audio", None);
    let proto = ProtocolObject::from_ref(&*output);
    // SAFETY: `output` implements `SCStreamOutput` and outlives the stream,
    // which is stopped below before either is dropped.
    let added = unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            proto,
            SCStreamOutputType::Audio,
            Some(&queue),
        )
    };
    if let Err(e) = added {
        *producer = output.reclaim(stats);
        return Err(format!("attaching to the desktop audio stream: {e}"));
    }

    let start_error: std::sync::Arc<Mutex<Option<String>>> = Default::default();
    {
        let start_error = start_error.clone();
        let handler = RcBlock::new(move |error: *mut NSError| {
            if let Some(error) = std::ptr::NonNull::new(error) {
                // SAFETY: the system's error, live for this call.
                let message = unsafe { error.as_ref() }.localizedDescription().to_string();
                *start_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
            }
        });
        // SAFETY: the handler is a block taking one `NSError *`.
        unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    }

    let result = supervise(&output, stop, started, &start_error);

    // Stopped before anything is dropped: the delegate is called from the
    // queue, and `stopCapture` is what guarantees it will not be called again.
    // The wait is what makes that guarantee land before the producer is taken
    // back below.
    let stopped = std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    {
        let stopped = stopped.clone();
        let handler = RcBlock::new(move |_error: *mut NSError| {
            let (lock, signal) = &*stopped;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            signal.notify_all();
        });
        // SAFETY: as above.
        unsafe { stream.stopCaptureWithCompletionHandler(Some(&handler)) };
    }
    {
        let (lock, signal) = &*stopped;
        let mut done = lock.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !*done && Instant::now() < deadline {
            let (next, _) = signal
                .wait_timeout(done, Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner());
            done = next;
        }
    }

    *producer = output.reclaim(stats);
    result
}

/// Waits for the stream to start delivering, then watches its pulse.
fn supervise(
    output: &Output,
    stop: &AtomicBool,
    started: impl FnOnce(),
    start_error: &Mutex<Option<String>>,
) -> Result<(), String> {
    let mut seen = 0;
    let mut last_pulse = Instant::now();
    let mut announced = Some(started);
    // Opening is not instant — the permission check, the stream, and the audio
    // server all have to agree — so the first buffer is given longer than a
    // running stream would be.
    let opening = Instant::now() + Duration::from_secs(5);
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(TICK);
        if let Some(message) = start_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            return Err(format!("starting the desktop audio stream: {message}"));
        }
        let cycles = output.ivars().cycles.load(Ordering::Relaxed);
        if cycles != seen {
            seen = cycles;
            last_pulse = Instant::now();
            if let Some(started) = announced.take() {
                started();
            }
        } else if announced.is_none() && last_pulse.elapsed() >= SILENT_LIMIT {
            return Err("the desktop audio stream stopped delivering".to_string());
        } else if announced.is_some() && Instant::now() > opening {
            return Err("the desktop audio stream never started delivering".to_string());
        }
    }
    Ok(())
}

impl Output {
    fn new(producer: Producer<f32>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars {
            inner: Mutex::new(Sink {
                producer: Some(producer),
                frames: Vec::new(),
                list: Vec::new(),
                dropped: 0,
            }),
            cycles: AtomicU64::new(0),
        });
        // SAFETY: `NSObject`'s designated initialiser.
        unsafe { msg_send![super(this), init] }
    }

    /// Takes the producer back and folds the delegate's tally into `stats`.
    ///
    /// Called only after `stopCaptureWithCompletionHandler` has reported, so
    /// nothing is still writing.
    fn reclaim(&self, stats: &CaptureStats) -> Producer<f32> {
        let mut sink = self.ivars().inner.lock().unwrap_or_else(|e| e.into_inner());
        stats.add_dropped_samples(sink.dropped);
        sink.dropped = 0;
        sink.producer.take().unwrap_or_else(empty_producer)
    }
}

/// A producer with nowhere to go, so the real one can be lent out and swapped
/// back without an `Option` in the caller's signature.
fn empty_producer() -> Producer<f32> {
    rtrb::RingBuffer::<f32>::new(1).0
}

/// The stream's configuration: audio we want, video we are obliged to carry.
fn configuration() -> Retained<SCStreamConfiguration> {
    let config = unsafe { SCStreamConfiguration::new() };
    // SAFETY: plain property sets on a fresh configuration.
    unsafe {
        config.setCapturesAudio(true);
        // The whole reason this module exists. Without it every cue and every
        // spoken chat message goes back out on the stream.
        config.setExcludesCurrentProcessAudio(true);
        config.setSampleRate(SAMPLE_RATE as isize);
        config.setChannelCount(CHANNELS as isize);
        config.setWidth(VIDEO_EDGE);
        config.setHeight(VIDEO_EDGE);
        // One frame a second of a 2x2 region, never collected.
        config.setMinimumFrameInterval(CMTime {
            value: 1,
            timescale: 1,
            flags: CMTimeFlags(1),
            epoch: 0,
        });
        config.setQueueDepth(3);
    }
    config
}

/// A filter over the whole of the first display.
///
/// **This is where the Screen Recording permission is asked for**, because
/// [`SCShareableContent`] is the first call that needs it, and it does not come
/// back until the user has answered. A refusal arrives as an error rather than
/// as empty content, so it can be reported in the user's terms.
fn filter() -> Result<Retained<SCContentFilter>, String> {
    let content = shareable_content()?;
    // SAFETY: reading a property of the content we were handed.
    let displays = unsafe { content.displays() };
    let Some(display) = displays.iter().next() else {
        return Err("this Mac reports no display to capture audio alongside".to_string());
    };
    let no_windows: Retained<NSArray<SCWindow>> = NSArray::new();
    // SAFETY: the initialiser takes a display and an array of windows.
    Ok(unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &no_windows,
        )
    })
}

/// Asks the system what can be captured, blocking until it answers.
fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
    /// The answer, in a shape that can cross a thread.
    ///
    /// **The content comes back as an address rather than as a
    /// `Retained<SCShareableContent>`**, because a `Retained` is not `Send`: the
    /// handler runs on one of the system's queues, not this thread. The block
    /// retains it there and hands over the raw pointer of that +1 reference,
    /// which is reclaimed as a `Retained` below — the same balance, and the same
    /// device `tts/sapi_mac.rs` uses to get an object off the main thread.
    /// Retain and release are atomic, and the object's own API is only ever
    /// touched on this thread.
    struct Answer {
        content: Option<std::ptr::NonNull<SCShareableContent>>,
        error: Option<String>,
        done: bool,
    }
    // SAFETY: the only thing crossing is a pointer to a +1-retained
    // `SCShareableContent`, whose reference count is atomic; nothing calls into
    // the object from the queue.
    unsafe impl Send for Answer {}

    let answer = std::sync::Arc::new((
        Mutex::new(Answer {
            content: None,
            error: None,
            done: false,
        }),
        std::sync::Condvar::new(),
    ));
    {
        let answer = answer.clone();
        let handler = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                let (lock, signal) = &*answer;
                let mut answer = lock.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(content) = std::ptr::NonNull::new(content) {
                    // SAFETY: a borrowed reference, retained here so it survives
                    // the handler; the +1 is handed on as a raw pointer and
                    // reclaimed by the waiter below.
                    answer.content = unsafe { Retained::retain(content.as_ptr()) }
                        .and_then(|retained| {
                            std::ptr::NonNull::new(Retained::into_raw(retained))
                        });
                } else if let Some(error) = std::ptr::NonNull::new(error) {
                    // SAFETY: the system's error, live for this call.
                    answer.error =
                        Some(unsafe { error.as_ref() }.localizedDescription().to_string());
                }
                answer.done = true;
                signal.notify_all();
            },
        );
        // SAFETY: the handler is a block of the documented shape.
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    }
    let (lock, signal) = &*answer;
    let mut got = lock.lock().unwrap_or_else(|e| e.into_inner());
    let deadline = Instant::now() + CONTENT_TIMEOUT;
    while !got.done && Instant::now() < deadline {
        let (next, _) = signal
            .wait_timeout(got, Duration::from_millis(250))
            .unwrap_or_else(|e| e.into_inner());
        got = next;
    }
    if let Some(error) = got.error.take() {
        return Err(format!(
            "Desktop Audio needs permission to record the screen, which is how macOS \
             lets an app hear what other apps are playing. Grant it to Pubsplash in \
             System Settings > Privacy & Security > Screen & System Audio Recording, \
             then add the source again. ({error})"
        ));
    }
    let content = got
        .content
        .take()
        .ok_or_else(|| "macOS did not answer whether the screen can be captured".to_string())?;
    // SAFETY: the +1 reference the handler passed over; this takes ownership of
    // it, so the retain and the release stay balanced.
    unsafe { Retained::from_raw(content.as_ptr()) }
        .ok_or_else(|| "macOS returned nothing for the screen contents".to_string())
}

#[cfg(test)]
mod tests {
    use super::interleave;

    /// **The measurement that chose ScreenCaptureKit over a process tap**, kept
    /// runnable, and the check to satisfy if this is ever revisited.
    ///
    /// Runs a real Desktop Audio capture while *this* process plays a loud tone,
    /// and then while another process does. A correct run hears **nothing** of
    /// its own and **something** of the other: a silent result on its own proves
    /// only that the capture is dead, which is exactly how the tap route's
    /// verification came to pass taps that then leaked.
    ///
    /// Run it repeatedly — the failure it is guarding against is intermittent:
    ///
    /// ```text
    /// for i in $(seq 12); do cargo test the_desktop_stream_excludes_us \
    ///     -- --include-ignored --nocapture | grep peak; done
    /// ```
    /// Deliberately not a note of the equal-tempered scale, so music playing on
    /// the machine cannot land in the same bin.
    const PROBE_HZ: f32 = 1234.0;

    #[test]
    #[ignore = "needs the Screen Recording permission, a display, and plays audio"]
    fn the_desktop_stream_excludes_us() {
        use crate::audio::health::CaptureStats;
        use crate::audio::mixer::{CHANNELS, SAMPLE_RATE};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let rate = SAMPLE_RATE as usize;
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(rate * CHANNELS);
        let stop = Arc::new(AtomicBool::new(false));
        let stats = CaptureStats::new();
        let reader_stop = Arc::clone(&stop);
        let reader = std::thread::spawn(move || {
            let outcome = super::run(&mut producer, &reader_stop, &stats, || {});
            if let Err(e) = &outcome {
                println!("capture ended: {e}");
            }
            outcome
        });
        // The stream has to be up before anything is played at it.
        std::thread::sleep(Duration::from_secs(3));

        // **Drained first, every time.** The ring holds a second of audio and
        // nothing empties it between windows, so without this each window opens
        // by measuring the tail of the one before it -- which reads as a leak in
        // the control window and is the surest way to mismeasure this.
        let collect_over = |seconds: f32, consumer: &mut rtrb::Consumer<f32>| {
            while consumer.pop().is_ok() {}
            let mut got: Vec<f32> = Vec::new();
            let deadline = Instant::now() + Duration::from_secs_f32(seconds);
            while Instant::now() < deadline {
                while let Ok(sample) = consumer.pop() {
                    got.push(sample);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            got
        };
        let peak = |samples: &[f32]| samples.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        let ours_in = |samples: &[f32]| {
            super::tone_amplitude(samples, PROBE_HZ, SAMPLE_RATE as f32)
        };

        // A control window. Whatever else the machine is playing shows up here
        // too, which is exactly why the verdict is taken at [`PROBE_HZ`] rather
        // than from the level.
        let quiet = collect_over(1.5, &mut consumer);

        // Ours. Must not be heard.
        let tone: Vec<f32> = (0..rate * 3 * CHANNELS)
            .map(|i| {
                let t = (i / CHANNELS) as f32 / rate as f32;
                (t * PROBE_HZ * std::f32::consts::TAU).sin() * 0.8
            })
            .collect();
        let playing = Arc::new(AtomicBool::new(false));
        let player = {
            let playing = Arc::clone(&playing);
            std::thread::spawn(move || crate::audio::render::play_samples_until(&tone, &playing))
        };
        let ours = collect_over(2.0, &mut consumer);
        playing.store(true, Ordering::Relaxed);
        let _ = player.join();
        // Let the output go quiet again before the next window is opened.
        std::thread::sleep(Duration::from_millis(500));

        // Somebody else's. Must be heard, or the run above proved nothing.
        // Started and left running: the window has to be open *while* it plays,
        // and waiting for it first would drain the very audio being measured.
        let mut other = std::process::Command::new("afplay")
            .arg("/System/Library/Sounds/Sosumi.aiff")
            .spawn()
            .expect("afplay should start");
        let theirs = collect_over(1.5, &mut consumer);
        let _ = other.wait();

        stop.store(true, Ordering::Relaxed);
        let _ = reader.join();

        let (quiet_tone, ours_tone) = (ours_in(&quiet), ours_in(&ours));
        println!(
            "at {PROBE_HZ} Hz -- control {quiet_tone:.5}, while we played {ours_tone:.5}   \
             (levels: control {:.4}, ours {:.4}, theirs {:.4})",
            peak(&quiet),
            peak(&ours),
            peak(&theirs)
        );
        assert!(
            peak(&theirs) > 0.01,
            "the capture heard nothing of another app, so it proves nothing about exclusion"
        );
        // The control window is the floor: whatever else is playing contributes
        // to both windows equally, so only a rise above it is ours.
        assert!(
            ours_tone < quiet_tone.max(0.002) * 8.0,
            "Pubsplash's own tone reached the desktop capture: {ours_tone:.5} at {PROBE_HZ} Hz \
             against a control of {quiet_tone:.5}. That is every cue and every spoken message \
             going out on the stream."
        );
    }

    #[test]
    fn two_planes_become_frames() {
        let left = [1.0, 3.0, 5.0];
        let right = [2.0, 4.0, 6.0];
        let mut out = Vec::new();
        interleave(&[&left, &right], &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    }

    /// Reading planar audio as interleaved is speech at half speed in one ear
    /// rather than a crash, so the mono case is pinned too.
    #[test]
    fn one_plane_reaches_both_ears() {
        let mono = [0.5, -0.25];
        let mut out = Vec::new();
        interleave(&[&mono], &mut out);
        assert_eq!(out, vec![0.5, 0.5, -0.25, -0.25]);
    }

    #[test]
    fn a_wider_stream_is_taken_as_its_first_pair() {
        let (a, b, c) = ([1.0], [2.0], [3.0]);
        let mut out = Vec::new();
        interleave(&[&a, &b, &c], &mut out);
        assert_eq!(out, vec![1.0, 2.0]);
    }

    /// Planes of different lengths are the shorter of the two, never a read off
    /// the end of one of them.
    #[test]
    fn ragged_planes_are_cut_to_the_shortest() {
        let left = [1.0, 3.0, 9.0];
        let right = [2.0, 4.0];
        let mut out = Vec::new();
        interleave(&[&left, &right], &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn nothing_in_is_nothing_out() {
        let mut out = vec![9.0, 9.0];
        interleave(&[], &mut out);
        assert!(out.is_empty());
    }

    /// The buffer is reused across buffers, so it must not carry the last one's
    /// tail into a shorter one.
    #[test]
    fn a_reused_buffer_does_not_keep_the_previous_frames() {
        let mut out = Vec::new();
        interleave(&[&[1.0, 2.0, 3.0][..], &[1.0, 2.0, 3.0][..]], &mut out);
        assert_eq!(out.len(), 6);
        interleave(&[&[7.0][..], &[8.0][..]], &mut out);
        assert_eq!(out, vec![7.0, 8.0]);
    }
}
