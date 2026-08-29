//! Streaming file decode: the one place Pubsplash turns a music file on disk
//! into interleaved stereo f32 at [`SAMPLE_RATE`], packet by packet.
//!
//! Both sources that play a file from the user's own disk go through here — the
//! Media Player and the Media Scheduler — and neither one holds a track in
//! memory whole: five minutes of 48 kHz stereo f32 is 115 MB, and a library
//! folder is hours of it. Each packet is widened to stereo and resampled
//! through a [`StereoStream`] that carries its interpolation state across
//! packets, because resampling each packet on its own puts a click at every
//! packet boundary, forty times a second.
//!
//! This is deliberately *not* [`crate::audio::convert::decode_audio`], the
//! whole-buffer door every other input goes through. It is the exception that
//! module's header names: the file is the user's own music rather than a speech
//! API's answer or a sound pack's asset, so it may be an hour long, and it is
//! read straight through symphonia. That is also why `.opus` is not playable
//! here — no released symphonia decodes it — and why
//! [`super::SUPPORTED_EXTENSIONS`] is symphonia's coverage and nothing more.
//!
//! The caller supplies the sink and decides when to stop. Returning
//! [`ControlFlow::Break`] carries the caller's own reason back out through
//! [`stream_file`], which is what lets the Media Player answer a skip mid-track
//! without this module knowing what a skip is.

use crate::audio::convert::{StereoStream, convert_to_stereo};
use crate::audio::mixer::SAMPLE_RATE;
use std::ops::ControlFlow;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Decodes `path`, handing `sink` interleaved stereo f32 at [`SAMPLE_RATE`] as
/// it goes.
///
/// - `Ok(None)` — the file played to its end.
/// - `Ok(Some(reason))` — `sink` broke out, with whatever it wanted to say.
/// - `Err(message)` — no audio at all could be decoded from the file.
///
/// The distinction in the error case is what a caller needs to tell "one broken
/// file in an otherwise good folder" from "the whole folder has gone away": a
/// file that played even partly is a success, because a torn tail is still
/// worth having heard.
pub fn stream_file<T>(
    path: &Path,
    sink: &mut impl FnMut(&[f32]) -> ControlFlow<T>,
) -> Result<Option<T>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            // Gapless playback trims the encoder padding an MP3 carries at both
            // ends, which is the difference between a seamless album and a click
            // between every track.
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| e.to_string())?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "the file has no audio track".to_string())?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| e.to_string())?;

    let mut buffer: Option<SampleBuffer<f32>> = None;
    let mut resampler: Option<(u32, StereoStream)> = None;
    let mut played_anything = false;

    // Symphonia reports a clean end of stream as an IO error, and a torn tail is
    // still worth having played, so any error out of `next_packet` ends the
    // track rather than failing it — which is what the `Ok` pattern here does.
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A damaged packet is skipped; the format layer has already
            // resynchronized by the time it says so.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => {
                if played_anything {
                    break;
                }
                return Err(e.to_string());
            }
        };
        let spec = *decoded.spec();
        let buffer =
            buffer.get_or_insert_with(|| SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
        buffer.copy_interleaved_ref(decoded);
        let channels = spec.channels.count().max(1);
        let stereo = convert_to_stereo(buffer.samples(), channels)?;

        let samples = if spec.rate == SAMPLE_RATE {
            stereo
        } else {
            // Rebuilt only if the file's rate actually changes mid-stream,
            // which is legal in a few containers and vanishingly rare.
            if !matches!(&resampler, Some((rate, _)) if *rate == spec.rate) {
                resampler = Some((spec.rate, StereoStream::new(spec.rate)));
            }
            match &mut resampler {
                Some((_, stream)) => stream.push(&stereo),
                None => unreachable!("a resampler was just installed"),
            }
        };
        if samples.is_empty() {
            continue;
        }
        played_anything = true;
        if let ControlFlow::Break(reason) = sink(&samples) {
            return Ok(Some(reason));
        }
    }

    // The resampler holds back the frame it still needed a neighbour for.
    if let Some((_, stream)) = &mut resampler {
        let tail = stream.finish();
        if !tail.is_empty() {
            played_anything = true;
            if let ControlFlow::Break(reason) = sink(&tail) {
                return Ok(Some(reason));
            }
        }
    }

    if played_anything {
        Ok(None)
    } else {
        Err("no audio could be decoded from it".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_is_not_there_is_an_error_rather_than_a_silent_success() {
        let mut sink = |_: &[f32]| ControlFlow::<()>::Continue(());
        assert!(stream_file(Path::new(r"Z:\nope\nothing.mp3"), &mut sink).is_err());
    }

    /// A file with no decodable audio must not read as "played to the end", or
    /// a caller counting failures would never notice a folder full of them.
    #[test]
    fn a_file_that_is_not_audio_is_an_error() {
        let path = std::env::temp_dir().join(format!("pubsplash-decode-{}.mp3", std::process::id()));
        std::fs::write(&path, b"this is not an MP3").unwrap();
        let mut sink = |_: &[f32]| ControlFlow::<()>::Continue(());
        assert!(stream_file(&path, &mut sink).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
