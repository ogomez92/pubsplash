//! The local system voice on macOS — the slot SAPI 5 fills on Windows.
//!
//! The engine id stays `sapi` (`engines::SAPI`) even here, and that is
//! deliberate rather than an oversight. Ids are stable strings decoupled from
//! display names precisely so a settings file written on one machine still
//! resolves on another, and `resolve_id` falls back to this engine for anything
//! unknown — so it is the one engine that must always exist. What the user is
//! shown is `engines::display_name`'s business, and that is where a Mac is told
//! it is looking at the system voice rather than at SAPI.
//!
//! What replaces the COM apartment is `AVSpeechSynthesizer`'s
//! `write(_:toBufferCallback:)`, which hands back `AVAudioPCMBuffer`s instead of
//! playing them — exactly the shape this module already produces for
//! [`ExternalFeeds`], so the surrounding design does not move: one worker thread
//! draining the same bounded drop-oldest queue, feeding the same named source.
//!
//! Three things about that API are not obvious and are all load-bearing here.
//!
//! **The callback is the only way to know the utterance ended.** `write` returns
//! immediately and the buffers arrive on a queue of AVFoundation's choosing; the
//! last call carries a buffer of **zero frames**, and that terminator is the
//! completion signal. There is no error path — a voice that fails simply never
//! delivers anything — so [`WRITE_TIMEOUT`] is what turns that into a reported
//! failure instead of a worker thread parked forever.
//!
//! **The buffers are in the voice's own format, not ours.** A macOS voice
//! renders at its own rate (22.05 kHz is typical), mono, and usually
//! *non-interleaved*. So [`interleave`] flattens whatever came back and
//! `audio::convert` does the rate and channel work — the same two functions
//! every other decode path in the app already goes through.
//!
//! **`AVSpeechSynthesizer` must outlive the call.** Dropping it while the
//! utterance is in flight cancels the write and the terminator never arrives, so
//! it is held until the wait is over.

use super::engine::SynthRequest;
use super::queue::Queue;
use crate::audio::ExternalFeeds;
use crate::audio::convert;
use crate::audio::mixer::SAMPLE_RATE;
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::{Retained, autoreleasepool};
use objc2_avf_audio::{
    AVAudioBuffer, AVAudioCommonFormat, AVAudioPCMBuffer, AVSpeechSynthesisVoice,
    AVSpeechSynthesizer, AVSpeechUtterance,
};
use objc2_foundation::NSString;
use std::ptr::NonNull;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long to wait for an utterance's buffers before giving up on it.
///
/// A ceiling on a pathological case, not a budget: synthesis of a chat message
/// takes tens of milliseconds. It exists because the API reports failure by
/// staying silent — there is no error callback — so without it a voice that
/// cannot render would park the worker thread and every message behind it.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Installed system voice display names.
///
/// Every voice for every language, which is what a Mac has: the list is long
/// where Windows' is short. Deduplicated by name, because several languages ship
/// a voice of the same name and [`find_voice`] can only resolve a name to one of
/// them anyway — the same answer the Windows side gives for two tokens sharing a
/// description.
pub fn voice_names() -> Vec<String> {
    autoreleasepool(|_| {
        let mut names: Vec<String> = Vec::new();
        // SAFETY: a class method taking nothing and returning an autoreleased
        // array, read inside the pool that owns it.
        let voices = unsafe { AVSpeechSynthesisVoice::speechVoices() };
        for voice in &voices {
            // SAFETY: `voice` is a live element of the array above.
            let name = unsafe { voice.name() }.to_string();
            if !name.is_empty() && !names.iter().any(|seen| seen == &name) {
                names.push(name);
            }
        }
        names
    })
}

/// One utterance for the speech worker.
#[derive(Debug, Clone)]
pub struct SapiRequest {
    pub synth: SynthRequest,
    /// Name of the TTS source in the audio engine to feed.
    pub source_name: String,
}

/// Starts the speech worker and returns the queue that feeds it.
///
/// A plain thread rather than the serial `DispatchQueue` the seam originally
/// guessed at: the work is one blocking synthesis after another, which is what a
/// thread is, and the queue below already provides the serialization a
/// `DispatchQueue` would have been for.
pub(super) fn start(
    feeds: ExternalFeeds,
    problems: super::speaker::Problems,
) -> Queue<SapiRequest> {
    let queue = Queue::new(super::speaker::QUEUE_DEPTH);
    let worker = queue.clone();
    let spawned = std::thread::Builder::new()
        .name("system-voice".into())
        .spawn(move || speech_thread(worker, feeds, problems));
    if let Err(error) = spawned {
        log::error!("Could not start the system voice thread: {error}");
    }
    queue
}

fn speech_thread(
    queue: Queue<SapiRequest>,
    feeds: ExternalFeeds,
    problems: super::speaker::Problems,
) {
    // Whether the *name* we were asked for came back with nothing, remembered
    // for the reason the Windows side remembers it: `find_voice` walks every
    // installed voice, and a chat flood addressed to a voice that is not
    // installed would otherwise repeat that walk once per message.
    let mut current_voice = String::new();
    let mut current_id: Option<String> = None;

    while let Some(request) = queue.pop() {
        let synth = &request.synth;
        if synth.voice != current_voice {
            current_id = find_voice_identifier(&synth.voice);
            if current_id.is_none() && !synth.voice.is_empty() {
                log::warn!("Voice {:?} not found; using the default", synth.voice);
            }
            current_voice = synth.voice.clone();
        }
        let voice_missing = current_id.is_none() && !synth.voice.is_empty();

        let rendered = synth_to_pcm(synth, current_id.as_deref());

        // The API tab's tally. The system voice is local and free, so there is
        // nothing to bill and no model to name — but the request and character
        // counts are still what tell a user which source is doing the talking. A
        // voice that was asked for and missing is recorded as none, because the
        // default is what actually spoke.
        super::usage::record(
            super::engines::SAPI,
            synth.text.chars().count(),
            None,
            (!voice_missing && !synth.voice.is_empty()).then(|| synth.voice.clone()),
            rendered.is_err(),
        );
        match rendered {
            // `feed_all` sleeps while the ring drains, which is exactly why this
            // is its own thread.
            Ok(samples) => feeds.feed_all(&request.source_name, &samples, "TTS"),
            Err(e) => {
                // The local voice has no other way out, so a failure here is
                // silence rather than a degraded stream — worth telling the user
                // about, at the same one-a-minute rate as the network engines.
                log::error!("System voice synthesis failed: {e}");
                problems.report(
                    super::engines::SAPI,
                    &super::engine::TtsError::Other(e.clone()),
                );
            }
        }
    }
}

/// Synthesizes one phrase on the calling thread, for the voice preview.
///
/// The worker's queue feeds a mixer source, which a preview has no business
/// touching — the source may not exist yet, and previewing must never reach the
/// stream. So this synthesizes on the spot and hands the samples back for
/// [`crate::audio::cue`] to play, exactly as the network engines' preview does.
///
/// It needs no apartment of its own, which is the one piece of ceremony the
/// Windows twin has and this does not.
pub fn synth_preview(request: &SynthRequest) -> Result<Vec<f32>, super::engine::TtsError> {
    let voice_id = find_voice_identifier(&request.voice);
    synth_to_pcm(request, voice_id.as_deref()).map_err(super::engine::TtsError::Other)
}

/// What the buffer callback accumulates: the voice's own samples, interleaved,
/// plus the format they are in.
#[derive(Default)]
struct Rendered {
    samples: Vec<f32>,
    rate: f64,
    channels: usize,
    /// Set when a buffer arrives in a format [`interleave`] does not read, so
    /// the failure names the format rather than returning silence.
    unsupported: Option<String>,
}

/// Everything one in-flight utterance owns on the main thread.
///
/// Boxed and handed back to the worker as an address, because none of these
/// three are `Send` and all three must be dropped where they were made. The
/// worker never dereferences it — it only posts it back to the main queue to be
/// released once the wait is over.
struct MainThreadHeld {
    _callback: RcBlock<dyn Fn(NonNull<AVAudioBuffer>)>,
    _synthesizer: Retained<AVSpeechSynthesizer>,
    _utterance: Retained<AVSpeechUtterance>,
}

/// Renders text to 48 kHz stereo f32 samples through `AVSpeechSynthesizer`.
///
/// **The synthesis runs on the main thread and the waiting runs here**, which is
/// not a preference — see the module header. `writeUtterance` delivers its
/// buffers through the *main* run loop and nowhere else; a worker thread pumping
/// its own run loop receives nothing at all, which was measured rather than
/// assumed.
///
/// That still keeps the app's rule that the interface never blocks on audio. The
/// main thread is not made to wait for anything: it services a hundred-odd
/// buffer callbacks, each a `memcpy`, interleaved with its own events, and
/// finishes far ahead of real time (about 0.2 s of work for 4 s of speech). This
/// thread does the waiting, and every caller of this function is already a
/// worker — the speech thread above and `ui::scenes`' `tts-preview` thread — so
/// nothing here can deadlock against the thread it is dispatching to.
///
/// The cost is that speech stalls while the main thread is blocked by something
/// else, which is the other half of what [`WRITE_TIMEOUT`] is for.
fn synth_to_pcm(request: &SynthRequest, voice_id: Option<&str>) -> Result<Vec<f32>, String> {
    let rendered = Arc::new(Mutex::new(Rendered::default()));
    let (finished, wait) = mpsc::channel::<()>();
    // The address of this utterance's main-thread objects, sent back so they can
    // be released on the thread that made them.
    let (held_tx, held_rx) = mpsc::channel::<usize>();

    let collector = Arc::clone(&rendered);
    // Cloned out because the closure below has to be `'static`.
    let text = request.text.clone();
    let voice_id = voice_id.map(str::to_string);
    let (rate, volume, pitch) = (request.rate, request.volume, request.pitch);

    DispatchQueue::main().exec_async(move || {
        let callback = RcBlock::new(move |buffer: NonNull<AVAudioBuffer>| {
            // SAFETY: AVFoundation owns this buffer for the duration of the call.
            let buffer = unsafe { buffer.as_ref() };
            let Some(pcm) = buffer.downcast_ref::<AVAudioPCMBuffer>() else {
                return;
            };
            // SAFETY: `pcm` is a live buffer; this is a plain accessor.
            let frames = unsafe { pcm.frameLength() } as usize;
            if frames == 0 {
                // The terminator: zero frames means the utterance is complete. A
                // send that fails means the waiter timed out and went home,
                // which is not this side's problem.
                let _ = finished.send(());
                return;
            }
            // Poisoning cannot happen -- nothing under this lock panics -- but
            // must not panic *here* if it somehow did: this is AVFoundation's
            // thread and an unwind through it is not ours to take.
            let Ok(mut collector) = collector.lock() else {
                return;
            };
            if let Err(e) = interleave(pcm, frames, &mut collector) {
                collector.unsupported = Some(e);
            }
        });

        autoreleasepool(|_| {
            let text = NSString::from_str(&text);
            // SAFETY: a class method over a live string.
            let utterance = unsafe { AVSpeechUtterance::speechUtteranceWithString(&text) };
            // SAFETY: plain setters on a live utterance; every value is clamped
            // into the range AVFoundation documents.
            unsafe {
                // A lookup, not a walk -- see `find_voice_identifier`. A voice
                // that has gone away since it was resolved answers `None`, which
                // leaves the default in place, which is what the worker below
                // would have chosen anyway.
                if let Some(voice) = voice_id
                    .as_deref()
                    .and_then(|id| AVSpeechSynthesisVoice::voiceWithIdentifier(&NSString::from_str(id)))
                {
                    utterance.setVoice(Some(&voice));
                }
                utterance.setRate(av_rate(rate));
                utterance.setVolume(volume.clamp(0, 100) as f32 / 100.0);
                utterance.setPitchMultiplier(av_pitch(pitch));
            }

            // SAFETY: `AVSpeechSynthesizer::new` takes nothing.
            let synthesizer = unsafe { AVSpeechSynthesizer::new() };
            // SAFETY: the block and the synthesizer are kept alive past this
            // call by the box below; dropping either here would cancel the write
            // and the terminator would never arrive.
            unsafe {
                synthesizer
                    .writeUtterance_toBufferCallback(&utterance, RcBlock::as_ptr(&callback));
            }
            let held = Box::into_raw(Box::new(MainThreadHeld {
                _callback: callback,
                _synthesizer: synthesizer,
                _utterance: utterance,
            }));
            let _ = held_tx.send(held as usize);
        });
    });

    let timed_out = wait.recv_timeout(WRITE_TIMEOUT).is_err();

    // Released on the main thread, where it was made, and after the wait either
    // way: on a timeout this is also what cancels a write still in flight.
    if let Ok(held) = held_rx.recv_timeout(WRITE_TIMEOUT) {
        DispatchQueue::main().exec_async(move || {
            // SAFETY: this address came from `Box::into_raw` on this same queue,
            // is read exactly once, and nothing else holds it.
            drop(unsafe { Box::from_raw(held as *mut MainThreadHeld) });
        });
    }

    if timed_out {
        return Err(format!(
            "the system voice did not answer within {} seconds; \
             the interface may be busy",
            WRITE_TIMEOUT.as_secs()
        ));
    }

    let rendered = rendered
        .lock()
        .map_err(|_| "the synthesis buffer was poisoned".to_string())?;
    if let Some(unsupported) = &rendered.unsupported {
        return Err(unsupported.clone());
    }
    if rendered.samples.is_empty() {
        return Err("the system voice produced no audio".to_string());
    }
    let stereo = convert::convert_to_stereo(&rendered.samples, rendered.channels)?;
    Ok(convert::resample_stereo(
        &stereo,
        rendered.rate.round() as u32,
        SAMPLE_RATE,
    ))
}

/// Appends one buffer to `into`, interleaved, in the buffer's own rate and
/// channel count.
///
/// A macOS voice hands back **non-interleaved** planar audio as a rule — one
/// pointer per channel, `stride` samples apart — but the interleaved layout is
/// legal too, and there both channels live in the first plane. Both are handled
/// rather than assumed, because getting it wrong is not a crash: it is speech
/// that plays at the wrong speed or only in one ear.
fn interleave(pcm: &AVAudioPCMBuffer, frames: usize, into: &mut Rendered) -> Result<(), String> {
    // SAFETY: `pcm` is live, and these are its plain accessors.
    let (format, stride) = unsafe { (pcm.format(), pcm.stride()) };
    // SAFETY: as above.
    let (rate, channels, interleaved, common) = unsafe {
        (
            format.sampleRate(),
            format.channelCount() as usize,
            format.isInterleaved(),
            format.commonFormat(),
        )
    };
    if channels == 0 || rate <= 0.0 {
        return Err("the system voice reported a buffer with no format".to_string());
    }
    if into.channels == 0 {
        into.channels = channels;
        into.rate = rate;
    } else if into.channels != channels || into.rate != rate {
        // Every buffer of one utterance is in one format; a change mid-way would
        // silently corrupt the resample below, so it is refused rather than
        // averaged.
        return Err("the system voice changed format mid-utterance".to_string());
    }

    // The index of frame `frame`, channel `channel`, within plane `plane`.
    let locate = |frame: usize, channel: usize| -> (usize, usize) {
        if interleaved {
            (0, frame * stride + channel)
        } else {
            (channel, frame * stride)
        }
    };
    let planes = if interleaved { 1 } else { channels };

    match common {
        AVAudioCommonFormat::PCMFormatFloat32 => {
            // SAFETY: `floatChannelData` is non-null for a float buffer, and has
            // `planes` entries.
            let data = unsafe { pcm.floatChannelData() };
            if data.is_null() {
                return Err("the system voice returned an empty float buffer".to_string());
            }
            // SAFETY: `planes` pointers, by the layout rules above.
            let planes = unsafe { std::slice::from_raw_parts(data, planes) };
            into.samples.reserve(frames * channels);
            for frame in 0..frames {
                for channel in 0..channels {
                    let (plane, index) = locate(frame, channel);
                    // SAFETY: `index` is within the plane by the buffer's own
                    // frame count and stride.
                    into.samples.push(unsafe { *planes[plane].as_ptr().add(index) });
                }
            }
            Ok(())
        }
        AVAudioCommonFormat::PCMFormatInt16 => {
            // SAFETY: as above, for the int16 accessor.
            let data = unsafe { pcm.int16ChannelData() };
            if data.is_null() {
                return Err("the system voice returned an empty 16-bit buffer".to_string());
            }
            // SAFETY: `planes` pointers, by the layout rules above.
            let planes = unsafe { std::slice::from_raw_parts(data, planes) };
            into.samples.reserve(frames * channels);
            for frame in 0..frames {
                for channel in 0..channels {
                    let (plane, index) = locate(frame, channel);
                    // SAFETY: as above.
                    let sample = unsafe { *planes[plane].as_ptr().add(index) };
                    into.samples.push(f32::from(sample) / 32768.0);
                }
            }
            Ok(())
        }
        other => Err(format!(
            "the system voice returned an unreadable sample format ({})",
            other.0
        )),
    }
}

/// Resolves an installed voice's display name to its stable identifier
/// (case-insensitive). `None` for the empty string, meaning: keep the default.
///
/// **An identifier, not the voice object, and that is the point.** Resolving a
/// name walks every installed voice — on a Mac that is a hundred and fifty of
/// them, each one an Objective-C string to read and compare — and the voice
/// object it finds is not `Send`, so it could not cross to the thread that needs
/// it anyway. An identifier is a plain `String`: the worker resolves it once per
/// voice change and the main thread turns it back into a voice with
/// `voiceWithIdentifier`, which is a lookup rather than a walk.
///
/// The Windows twin caches the resolved token for exactly this reason, and its
/// comment records what happens without it: a full enumeration per chat message.
fn find_voice_identifier(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    autoreleasepool(|_| {
        // SAFETY: a class method taking nothing, read inside the pool that owns
        // the array it returns.
        let voices = unsafe { AVSpeechSynthesisVoice::speechVoices() };
        voices
            .iter()
            // SAFETY: `voice` is a live element of the array above.
            .find(|voice| unsafe { voice.name() }.to_string().eq_ignore_ascii_case(name))
            .map(|voice| unsafe { voice.identifier() }.to_string())
    })
}

/// Maps Pubsplash's `-10..=10` rate onto `AVSpeechUtterance`'s `0.0..=1.0`,
/// where the default is not the midpoint.
///
/// Two straight lines meeting at the default rather than one across the whole
/// span, so that **0 is the voice's normal speed** on both platforms — which is
/// the property a settings file carried between them depends on. A single linear
/// map would put 0 near the middle of the range instead, and every source would
/// change speed the first time its settings were opened on the other OS.
fn av_rate(rate: i32) -> f32 {
    // The constants are `AVSpeechUtteranceMinimum/Default/MaximumSpeechRate`,
    // written out rather than linked: they are `extern` statics, so reading them
    // is an `unsafe` block per call for three numbers that have not moved since
    // the API shipped.
    const MINIMUM: f32 = 0.0;
    const DEFAULT: f32 = 0.5;
    const MAXIMUM: f32 = 1.0;
    let rate = rate.clamp(-10, 10) as f32 / 10.0;
    if rate < 0.0 {
        DEFAULT + rate * (DEFAULT - MINIMUM)
    } else {
        DEFAULT + rate * (MAXIMUM - DEFAULT)
    }
}

/// Maps Pubsplash's `-50..=50` pitch onto `AVSpeechUtterance`'s `0.5..=2.0`.
///
/// Also two lines meeting at the default, and for the same reason: 0 has to be
/// the voice's own pitch. The multiplier is not symmetric about 1.0, so a single
/// line would make 0 mean "slightly low".
fn av_pitch(pitch: i32) -> f32 {
    const MINIMUM: f32 = 0.5;
    const DEFAULT: f32 = 1.0;
    const MAXIMUM: f32 = 2.0;
    let pitch = pitch.clamp(-50, 50) as f32 / 50.0;
    if pitch < 0.0 {
        DEFAULT + pitch * (DEFAULT - MINIMUM)
    } else {
        DEFAULT + pitch * (MAXIMUM - DEFAULT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::AnyThread;

    /// Every Mac ships voices, so an empty list is a broken enumeration rather
    /// than a bare machine. The Windows twin asserts the same thing.
    #[test]
    fn voice_enumeration_finds_installed_voices() {
        let voices = voice_names();
        assert!(
            !voices.is_empty(),
            "expected at least one installed system voice"
        );
    }

    /// The property a settings file carried between Windows and macOS depends
    /// on: rate 0 and pitch 0 mean "the voice's own", not "the middle of the
    /// range".
    #[test]
    fn the_neutral_rate_and_pitch_are_the_voices_own() {
        assert_eq!(av_rate(0), 0.5);
        assert_eq!(av_pitch(0), 1.0);
    }

    #[test]
    fn rate_and_pitch_span_the_full_range_and_clamp_beyond_it() {
        assert_eq!(av_rate(-10), 0.0);
        assert_eq!(av_rate(10), 1.0);
        assert_eq!(av_rate(-99), av_rate(-10));
        assert_eq!(av_rate(99), av_rate(10));

        assert_eq!(av_pitch(-50), 0.5);
        assert_eq!(av_pitch(50), 2.0);
        assert_eq!(av_pitch(-99), av_pitch(-50));
        assert_eq!(av_pitch(99), av_pitch(50));
    }

    #[test]
    fn rate_and_pitch_move_the_right_way() {
        assert!(av_rate(-5) < av_rate(0) && av_rate(0) < av_rate(5));
        assert!(av_pitch(-25) < av_pitch(0) && av_pitch(0) < av_pitch(25));
    }

    /// An unknown voice name resolves to nothing, which is what makes the
    /// worker's "fall back to the default" path reachable.
    #[test]
    fn an_unknown_voice_name_resolves_to_nothing() {
        assert!(find_voice_identifier("Not A Real Voice At All").is_none());
    }

    /// The empty string means "the default voice" and must not be matched
    /// against the installed list — a voice whose name is somehow empty would
    /// otherwise be selected for every source that never chose one.
    #[test]
    fn the_empty_voice_name_is_the_default_not_a_lookup() {
        assert!(find_voice_identifier("").is_none());
    }

    /// Every name the picker offers has to resolve, or a user could choose a
    /// voice that then silently falls back to the default.
    #[test]
    fn every_listed_voice_resolves_by_name() {
        for name in voice_names() {
            assert!(
                find_voice_identifier(&name).is_some(),
                "the picker offers {name:?} but it does not resolve"
            );
        }
    }

    /// Builds a real `AVAudioPCMBuffer` in a known layout and checks
    /// [`interleave`] reads it back in the right order.
    ///
    /// This is the part of the module that a wrong guess breaks *quietly* --
    /// planar audio read as interleaved is speech at half speed in one ear, not
    /// a crash -- and the one part that can be tested without a run loop, since
    /// constructing a buffer needs no synthesizer. See
    /// `the_system_voice_is_verified_in_the_app` for why the synthesis itself is
    /// not tested here.
    fn buffer(rate: f64, channels: u32, interleaved: bool, frames: u32) -> Retained<AVAudioPCMBuffer> {
        use objc2_avf_audio::AVAudioFormat;
        // SAFETY: plain constructors over primitives; the format is one
        // AVFoundation supports and the capacity is non-zero.
        unsafe {
            let format = AVAudioFormat::initWithCommonFormat_sampleRate_channels_interleaved(
                AVAudioFormat::alloc(),
                AVAudioCommonFormat::PCMFormatFloat32,
                rate,
                channels,
                interleaved,
            )
            .expect("a float32 format");
            let buffer =
                AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(AVAudioPCMBuffer::alloc(), &format, frames)
                    .expect("a PCM buffer");
            buffer.setFrameLength(frames);
            buffer
        }
    }

    /// Writes frame `f` of channel `c` as `f * 10 + c`, so a mis-read shows up
    /// as a recognisable number rather than as plausible-looking noise.
    fn fill(pcm: &AVAudioPCMBuffer, channels: usize, interleaved: bool, frames: usize) {
        // SAFETY: the buffer was made float32 just above, so `floatChannelData`
        // is non-null, and every index below is within the frame count it was
        // allocated for.
        unsafe {
            let stride = pcm.stride();
            let planes = std::slice::from_raw_parts(
                pcm.floatChannelData(),
                if interleaved { 1 } else { channels },
            );
            for frame in 0..frames {
                for channel in 0..channels {
                    let (plane, index) = if interleaved {
                        (0, frame * stride + channel)
                    } else {
                        (channel, frame * stride)
                    };
                    *planes[plane].as_ptr().add(index) = (frame * 10 + channel) as f32;
                }
            }
        }
    }

    #[test]
    fn planar_audio_is_read_in_frame_order() {
        // The layout a macOS voice actually hands back: separate planes.
        let pcm = buffer(22050.0, 2, false, 3);
        fill(&pcm, 2, false, 3);
        let mut rendered = Rendered::default();

        interleave(&pcm, 3, &mut rendered).expect("a float32 planar buffer should be read");

        assert_eq!(rendered.samples, vec![0.0, 1.0, 10.0, 11.0, 20.0, 21.0]);
        assert_eq!(rendered.channels, 2);
        assert_eq!(rendered.rate, 22050.0);
    }

    #[test]
    fn interleaved_audio_is_read_in_frame_order_too() {
        let pcm = buffer(22050.0, 2, true, 3);
        fill(&pcm, 2, true, 3);
        let mut rendered = Rendered::default();

        interleave(&pcm, 3, &mut rendered).expect("a float32 interleaved buffer should be read");

        assert_eq!(rendered.samples, vec![0.0, 1.0, 10.0, 11.0, 20.0, 21.0]);
    }

    /// The common case: one mono plane, which must come back untouched for
    /// `convert_to_stereo` to duplicate.
    #[test]
    fn mono_audio_comes_back_as_one_channel() {
        let pcm = buffer(22050.0, 1, false, 4);
        fill(&pcm, 1, false, 4);
        let mut rendered = Rendered::default();

        interleave(&pcm, 4, &mut rendered).expect("a mono buffer should be read");

        assert_eq!(rendered.samples, vec![0.0, 10.0, 20.0, 30.0]);
        assert_eq!(rendered.channels, 1);
    }

    /// An utterance arrives as many buffers, and they have to concatenate.
    #[test]
    fn successive_buffers_accumulate() {
        let mut rendered = Rendered::default();
        for _ in 0..3 {
            let pcm = buffer(22050.0, 1, false, 2);
            fill(&pcm, 1, false, 2);
            interleave(&pcm, 2, &mut rendered).expect("each buffer should be read");
        }

        assert_eq!(rendered.samples, vec![0.0, 10.0, 0.0, 10.0, 0.0, 10.0]);
    }

    /// A format change mid-utterance would silently corrupt the resample, so it
    /// is refused rather than averaged.
    #[test]
    fn a_format_change_mid_utterance_is_refused() {
        let mut rendered = Rendered::default();
        let first = buffer(22050.0, 1, false, 2);
        interleave(&first, 2, &mut rendered).expect("the first buffer sets the format");

        let second = buffer(48000.0, 1, false, 2);

        assert!(
            interleave(&second, 2, &mut rendered).is_err(),
            "a second rate must not be accepted into the same utterance"
        );
    }

    /// Why there is no end-to-end synthesis test here, recorded as a test so it
    /// is read rather than skipped.
    ///
    /// `AVSpeechSynthesizer::writeUtterance` delivers its buffers **only**
    /// through the main thread's run loop. Under `cargo test` the main thread is
    /// inside libtest waiting on worker threads and runs no loop at all, so a
    /// synthesis started from a test would never receive a single buffer and
    /// would sit out [`WRITE_TIMEOUT`] before failing — a thirty-second test that
    /// proves nothing about the code.
    ///
    /// It is verified in the running app instead, where there is a real run
    /// loop: add a TTS source, press Preview voice, and the log shows either the
    /// speech or the reason. The pieces that *can* be tested in isolation are
    /// above — the layout reading, which is where the quiet bugs live, and the
    /// rate and pitch mapping.
    #[test]
    fn the_system_voice_is_verified_in_the_app() {
        assert_eq!(WRITE_TIMEOUT, Duration::from_secs(30));
    }
}
