//! The local system voice on macOS — the slot SAPI 5 fills on Windows.
//!
//! **Not built yet.** This is the platform seam standing open, with the shape
//! the real implementation has to fit.
//!
//! The engine id stays `sapi` (`engines::SAPI`) even here, and that is
//! deliberate rather than an oversight. Ids are stable strings decoupled from
//! display names precisely so a settings file written on one machine still
//! resolves on another, and `resolve_id` falls back to this engine for anything
//! unknown — so it is the one engine that must always exist. What the user is
//! shown is `engines::display_name`'s business, and that is where a Mac would be
//! told it is looking at the system voice rather than at SAPI.
//!
//! What replaces the COM apartment: `AVSpeechSynthesizer`'s
//! `write(_:toBufferCallback:)`, which hands back `AVAudioPCMBuffer`s instead of
//! playing them — which is exactly the shape this module already produces for
//! [`ExternalFeeds`], so the surrounding design does not move. The apartment
//! thread becomes a serial `DispatchQueue`, and [`voice_names`] becomes
//! `AVSpeechSynthesisVoice.speechVoices()` rather than the registry query the
//! Windows side shells out for.
//!
//! Until then the local voice reports nothing installed and synthesizes nothing.
//! Every network engine is unaffected — all eight of them are HTTP or WebSocket
//! clients that need no platform code at all — so speech is not lost on macOS,
//! only the offline voice.

use super::engine::SynthRequest;
use super::queue::Queue;
use crate::audio::ExternalFeeds;

/// Installed system voice display names. Empty until the `AVSpeechSynthesisVoice`
/// enumeration exists, which reads to the UI as "no voices installed" — the same
/// state a Windows machine with no SAPI voices would be in, and already handled.
pub fn voice_names() -> Vec<String> {
    Vec::new()
}

/// One utterance for the speech worker.
///
/// The fields are unread until there is a worker to read them; the shape has to
/// match the Windows one because `speaker` builds these regardless of platform.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SapiRequest {
    pub synth: SynthRequest,
    /// Name of the TTS source in the audio engine to feed.
    pub source_name: String,
}

/// Hands back a queue nothing drains yet.
///
/// The queue is bounded and **drop-oldest**, which is what makes this safe to
/// leave unattended: a chat flood fills it and then discards its own stale
/// entries rather than growing without limit. So an unimplemented engine costs
/// a bounded amount of memory and nothing else.
pub(super) fn start(
    _feeds: ExternalFeeds,
    _problems: super::speaker::Problems,
) -> Queue<SapiRequest> {
    log::info!("The local system voice is not implemented on macOS yet; use a network engine");
    Queue::new(super::speaker::QUEUE_DEPTH)
}

/// Synthesizes one utterance for the voice-preview button.
pub fn synth_preview(_request: &SynthRequest) -> Result<Vec<f32>, super::engine::TtsError> {
    Err(super::engine::TtsError::Other(
        "the local system voice is not implemented on macOS yet".to_string(),
    ))
}
