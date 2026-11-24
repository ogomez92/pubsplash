//! The media player's fade-outs, and the delay line that makes them possible.
//!
//! A track fades out two ways: when the user abandons it (next, previous,
//! Enter on another entry, Open file, a new folder), and when it ends on its
//! own. Both are applied **by the player's worker thread to the samples it is
//! about to push**, so a fade costs the UI nothing and the engine nothing — the
//! mixer only ever sees audio that is already quieter.
//!
//! ## Why a delay line
//!
//! The ring already holds a quarter of a second that will be heard at full
//! volume whatever happens next (see `player::MAX_QUEUED`), so a fade can only
//! be applied to audio that has not gone into it yet. [`Tail`] keeps the last
//! [`FADE_FRAMES`] of decoded audio back from the ring, which gives both kinds
//! of fade something to work on the moment they are wanted: a skip fades what
//! is held and drops the rest, and the end of a file fades what is held instead
//! of letting it out at full level. Decoding runs far ahead of real time, so
//! holding a second and a half back delays nothing anyone can hear.
//!
//! ## Fading where the song ends, not where the file ends
//!
//! Plenty of files end in seconds of silence, and fading the last stretch of
//! *the file* would fade nothing and then leave the dead air in. So the delay
//! line also holds back a trailing run of near-silence — up to
//! [`MAX_HELD_SILENCE_FRAMES`] of it — and lets it out only when sound follows
//! it, which is a pause in the middle of a song and is played untouched. When
//! the file ends with that run still held, the run is dropped and the fade
//! lands on the last audible stretch, which is where the song really ended.
//! Nothing in the middle of a track is ever altered.

use crate::audio::mixer::{CHANNELS, SAMPLE_RATE};
use std::collections::VecDeque;

/// How long a fade-out lasts, in frames: a second and a half.
pub const FADE_FRAMES: usize = SAMPLE_RATE as usize * 3 / 2;

/// The level below which a frame counts as silence: about -54 dBFS. Well under
/// anything meant to be heard and well over an encoder's dither, so a song's
/// last note has faded below it while the padding after it never rises above.
const SILENCE: f32 = 0.002;

/// The most trailing silence held back at once, so a file with minutes of it
/// (a hidden track at the end of an album) cannot grow the delay line without
/// bound. Silence longer than this is let out as it comes; it is real content
/// at that point, or near enough, and only this much of it can be trimmed.
const MAX_HELD_SILENCE_FRAMES: usize = SAMPLE_RATE as usize * 10;

/// The decoded audio a player has not pushed into the ring yet.
#[derive(Default)]
pub struct Tail {
    held: VecDeque<f32>,
    /// How many frames at the end of `held` are below [`SILENCE`].
    silent_frames: usize,
    /// Every sample taken in since the track started, held or not, so a short
    /// track's fade can be sized by the track rather than by what is left.
    taken: usize,
}

impl Tail {
    pub fn clear(&mut self) {
        self.held.clear();
        self.silent_frames = 0;
        self.taken = 0;
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Takes in a freshly decoded packet.
    pub fn push(&mut self, samples: &[f32]) {
        let frames = samples.len() / CHANNELS;
        let trailing = samples
            .chunks_exact(CHANNELS)
            .rev()
            .take_while(|frame| frame.iter().all(|s| s.abs() < SILENCE))
            .count();
        self.silent_frames = if trailing == frames {
            self.silent_frames + frames
        } else {
            trailing
        };
        self.held.extend(samples);
        self.taken += samples.len();
    }

    /// How many samples may go to the mixer now: everything but the fade's
    /// worth and the trailing silence.
    pub fn ready(&self) -> usize {
        let keep = (FADE_FRAMES + self.silent_frames.min(MAX_HELD_SILENCE_FRAMES)) * CHANNELS;
        self.held.len().saturating_sub(keep)
    }

    /// Up to `max` samples from the front, in one slice.
    pub fn front(&mut self, max: usize) -> &[f32] {
        let all = self.held.make_contiguous();
        &all[..all.len().min(max)]
    }

    /// Forgets the first `count` samples, which have gone to the mixer.
    pub fn consume(&mut self, count: usize) {
        self.held.drain(..count.min(self.held.len()));
        self.silent_frames = self.silent_frames.min(self.held.len() / CHANNELS);
    }

    /// The track is being abandoned: what is held, faded to nothing over
    /// [`FADE_FRAMES`], and the rest dropped.
    pub fn fade_out_now(&mut self) -> Vec<f32> {
        let take = self.held.len().min(FADE_FRAMES * CHANNELS);
        let mut out: Vec<f32> = self.held.drain(..take).collect();
        self.clear();
        let frames = out.len() / CHANNELS;
        apply_fade(&mut out, frames);
        out
    }

    /// The file has ended: the trailing silence dropped, and the last stretch
    /// of sound faded out. A track shorter than two fades is faded over its
    /// second half, so a short jingle is not faded from its first note.
    pub fn fade_out_ending(&mut self) -> Vec<f32> {
        let silent = self.silent_frames * CHANNELS;
        let audible = self.held.len().saturating_sub(silent);
        let track_frames = self.taken.saturating_sub(silent) / CHANNELS;
        let mut out: Vec<f32> = self.held.drain(..audible).collect();
        self.clear();
        let frames = (out.len() / CHANNELS).min(FADE_FRAMES).min(track_frames / 2);
        apply_fade(&mut out, frames.max(1));
        out
    }
}

/// Fades the last `frames` frames of `samples` to silence.
///
/// A raised cosine: it leaves full level without a step and arrives at zero
/// without one, so neither end of the fade is heard as an edge.
fn apply_fade(samples: &mut [f32], frames: usize) {
    let start = samples.len().saturating_sub(frames * CHANNELS);
    for (index, sample) in samples[start..].iter_mut().enumerate() {
        let t = (index / CHANNELS) as f32 / frames as f32;
        *sample *= 0.5 + 0.5 * (std::f32::consts::PI * t).cos();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, level: f32) -> Vec<f32> {
        vec![level; frames * CHANNELS]
    }

    #[test]
    fn a_fade_worth_of_audio_is_always_held_back() {
        let mut tail = Tail::default();
        tail.push(&tone(FADE_FRAMES / 2, 0.5));
        assert_eq!(tail.ready(), 0, "nothing goes out before a fade's worth is held");
        tail.push(&tone(FADE_FRAMES, 0.5));
        assert_eq!(tail.ready(), FADE_FRAMES / 2 * CHANNELS);
    }

    #[test]
    fn silence_is_held_until_sound_follows_it() {
        let mut tail = Tail::default();
        tail.push(&tone(FADE_FRAMES, 0.5));
        tail.push(&tone(1_000, 0.0));
        assert_eq!(tail.ready(), 0, "a pause might be the end of the song");
        tail.push(&tone(10, 0.5));
        // Sound came back, so the pause was inside the song and goes out as is.
        assert_eq!(tail.ready(), 1_010 * CHANNELS);
    }

    #[test]
    fn the_ending_fade_lands_on_the_last_sound_and_drops_the_silence() {
        let mut tail = Tail::default();
        tail.push(&tone(FADE_FRAMES * 3, 0.5));
        tail.push(&tone(SAMPLE_RATE as usize * 4, 0.0));
        let ready = tail.ready();
        tail.consume(ready);
        let out = tail.fade_out_ending();
        assert_eq!(out.len(), FADE_FRAMES * CHANNELS, "the silence was played");
        assert!((out[0] - 0.5).abs() < 1e-3, "the fade started with a step");
        assert!(out[out.len() - 1].abs() < 1e-3, "the fade did not reach silence");
        assert!(tail.is_empty());
    }

    #[test]
    fn a_skip_fades_what_is_held_and_drops_the_rest() {
        let mut tail = Tail::default();
        tail.push(&tone(FADE_FRAMES * 2, 0.5));
        let out = tail.fade_out_now();
        assert_eq!(out.len(), FADE_FRAMES * CHANNELS);
        assert!(out.windows(2).all(|pair| pair[1] <= pair[0] + 1e-6), "not a fade out");
        assert!(tail.is_empty());
    }

    #[test]
    fn a_short_jingle_is_faded_over_its_second_half_only() {
        let mut tail = Tail::default();
        tail.push(&tone(1_000, 0.5));
        let out = tail.fade_out_ending();
        assert_eq!(out.len(), 1_000 * CHANNELS);
        assert_eq!(out[499 * CHANNELS], 0.5, "the first half was faded");
        assert!(out[999 * CHANNELS] < 0.01);
    }
}
