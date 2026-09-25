//! Finding ffmpeg, and asking it what it can do.
//!
//! # Why there is an external encoder at all
//!
//! Pubsplash's own output is MP3 and only MP3 (`audio/encoder.rs`), which is
//! everything Icecast wants and nothing YouTube will take. YouTube's RTMP ingest
//! requires **H.264 video and AAC audio**, and it requires the video: an
//! audio-only source is refused outright, so a radio-style broadcast has to
//! carry a picture whether anyone looks at it or not. That is three codecs and a
//! protocol — AAC, H.264, FLV muxing, RTMP — none of which the app has, and two
//! of which are large, patent-adjacent C libraries.
//!
//! ffmpeg does all four, is already on a great many machines, and — being a
//! separate process — cannot take the app down when a stream goes wrong. The
//! cost is that it has to be *found*, which is what this module is.
//!
//! # What is looked for, and in what order
//!
//! 1. The path in Preferences, if the user set one. An explicit choice wins over
//!    everything, including a copy Pubsplash downloaded itself.
//! 2. `<data dir>\tools\ffmpeg.exe`, where [`install`] puts one.
//! 3. `ffmpeg.exe` on `PATH`, which is where winget, Chocolatey, Scoop and a
//!    manual install all end up.
//!
//! # Why the capabilities are probed rather than assumed
//!
//! An ffmpeg is not an ffmpeg. The common Windows builds come in GPL flavours
//! (which carry `libx264`) and LGPL ones (which carry `libopenh264` instead),
//! and a distribution may have neither. Choosing the encoder from a list the
//! binary itself prints means whichever build the user already has is used
//! correctly, and a build that genuinely cannot do the job says so at Connect —
//! in a dialog, with the user waiting — rather than as a dead stream later.

pub mod install;

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where [`install`] puts a downloaded copy, under the data directory.
pub const TOOLS_DIR: &str = "tools";
pub const EXE_NAME: &str = "ffmpeg.exe";

/// What one ffmpeg binary can do, as it reported itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The first line of `ffmpeg -version`, logged so a bug report says which
    /// build produced it.
    pub version: String,
    /// The H.264 encoder to ask for. See [`H264_ENCODERS`].
    pub h264: &'static str,
    /// The AAC encoder to ask for; ffmpeg's built-in `aac` unless the build
    /// carries Fraunhofer's, which sounds better at the bitrates in play.
    pub aac: &'static str,
}

/// H.264 encoders worth using, best first.
///
/// `libx264` is in every GPL build and is what OBS uses. `libopenh264` is
/// Cisco's and is what LGPL builds carry instead. `h264_nvenc` and the other
/// hardware encoders are deliberately absent: this encodes a *still picture* at
/// two frames a second, so there is nothing for a GPU to save, and a hardware
/// encoder that is present but unusable (no such GPU, driver too old) fails at
/// the point where the stream should be starting.
const H264_ENCODERS: [&str; 2] = ["libx264", "libopenh264"];

/// AAC encoders worth using, best first. `aac` is built in to every ffmpeg.
const AAC_ENCODERS: [&str; 2] = ["libfdk_aac", "aac"];

/// Where a copy Pubsplash downloaded itself lives.
pub fn managed_path() -> PathBuf {
    crate::data_dir::root().join(TOOLS_DIR).join(EXE_NAME)
}

/// Finds an ffmpeg to use, or explains what to do about it.
///
/// `configured` is the Preferences field, which is normally empty.
pub fn locate(configured: &str) -> Result<PathBuf, String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let path = PathBuf::from(configured);
        // An explicit setting that is wrong is reported rather than skipped: a
        // silent fall through to some other ffmpeg would leave the user looking
        // at a path that is not being used and no sign of it.
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "The FFmpeg location in Preferences, Audio names {}, which is not a file. \
                 Correct it, clear it to search automatically, or download a copy.",
                path.display()
            ))
        };
    }
    let managed = managed_path();
    if managed.is_file() {
        return Ok(managed);
    }
    if let Some(found) = on_path() {
        return Ok(found);
    }
    Err(
        "FFmpeg was not found. Streaming to YouTube needs it, because YouTube takes \
         H.264 video and AAC audio and Pubsplash produces neither on its own. \
         Use Preferences, Audio to download a copy or to point at one you already have."
            .to_string(),
    )
}

/// Searches `PATH` for `ffmpeg.exe`.
///
/// Hand-rolled rather than shelled out to `where`: spawning a console process
/// from a windowed app flashes a window, and this is called on the UI thread.
pub fn on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(EXE_NAME))
        .find(|candidate| candidate.is_file())
}

/// Runs `ffmpeg` and reads back what it can encode.
///
/// Blocking, and two process launches — about 50 ms measured against a current
/// build. That is cheap enough for a deliberate action the user is waiting on
/// (Connect, and refreshing the Preferences status line) and much too expensive
/// for anything repeating, so it must not go on a timer or on a keystroke. The
/// Preferences box that names the path deliberately re-reads on focus leaving
/// it rather than as it is typed, for exactly that reason.
pub fn probe(exe: &Path) -> Result<Capabilities, String> {
    let version = run(exe, &["-hide_banner", "-version"])?;
    let version = version
        .lines()
        .next()
        .unwrap_or("ffmpeg (version unknown)")
        .trim()
        .to_string();

    let encoders = run(exe, &["-hide_banner", "-encoders"])?;
    let h264 = pick(&encoders, &H264_ENCODERS).ok_or_else(|| {
        format!(
            "{} has no H.264 encoder, so it cannot send video to YouTube — and YouTube \
             refuses an audio-only stream. Install a build with libx264 or libopenh264, \
             or let Pubsplash download one.",
            exe.display()
        )
    })?;
    let aac = pick(&encoders, &AAC_ENCODERS).ok_or_else(|| {
        format!(
            "{} has no AAC encoder, which YouTube requires for audio.",
            exe.display()
        )
    })?;
    Ok(Capabilities {
        version,
        h264,
        aac,
    })
}

/// Runs ffmpeg and hands back everything it printed.
///
/// stdout and stderr are joined because ffmpeg is inconsistent about which it
/// uses — `-version` goes to stdout, most else to stderr — and every caller here
/// wants the text rather than the stream it arrived on.
fn run(exe: &Path, args: &[&str]) -> Result<String, String> {
    let output = command(exe)
        .args(args)
        .output()
        .map_err(|e| format!("Could not run {}: {e}", exe.display()))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if text.trim().is_empty() {
        return Err(format!(
            "{} ran but printed nothing, so it does not look like ffmpeg.",
            exe.display()
        ));
    }
    Ok(text)
}

/// Builds a `Command` that will not flash a console window.
///
/// Release builds set `windows_subsystem = "windows"`, so the app has no console
/// of its own; without `CREATE_NO_WINDOW` every ffmpeg launch pops a black box
/// on screen and steals focus — which for a screen-reader user means the focus
/// they were using is simply gone, mid-broadcast.
pub fn command(exe: &Path) -> Command {
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    // Only the Windows arm below mutates it, so every other platform sees a `mut`
    // that nothing uses.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut command = Command::new(exe);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    command
}

/// The first of `wanted` that appears in ffmpeg's encoder listing.
///
/// The listing's lines look like ` V....D libx264   libx264 H.264 ...`, so a
/// plain substring search would match an encoder named in another encoder's
/// *description* — `libx264rgb`'s line mentions `libx264`, and several decoders
/// name the encoder they pair with. Matching a whitespace-delimited field is
/// what makes the answer the encoder's own name.
fn pick(listing: &str, wanted: &[&'static str]) -> Option<&'static str> {
    wanted.iter().copied().find(|name| {
        listing
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real slice of `ffmpeg -encoders`, including the `libx264rgb` line that
    /// a substring search gets wrong.
    const LISTING: &str = "\
Encoders:
 V..... = Video
 ------
 V....D libx264              libx264 H.264 / AVC / MPEG-4 AVC (codec h264)
 V....D libx264rgb           libx264 H.264 / AVC / MPEG-4 AVC RGB (codec h264)
 A....D aac                  AAC (Advanced Audio Coding)
 A....D libmp3lame           libmp3lame MP3 (MPEG audio layer 3) (codec mp3)
";

    #[test]
    fn picks_the_encoder_by_its_own_name() {
        assert_eq!(pick(LISTING, &H264_ENCODERS), Some("libx264"));
        assert_eq!(pick(LISTING, &AAC_ENCODERS), Some("aac"));
    }

    /// An LGPL build has no libx264 and must fall through to Cisco's.
    #[test]
    fn falls_back_to_openh264() {
        let lgpl = " V....D libopenh264           OpenH264 H.264 (codec h264)\n A....D aac  AAC\n";
        assert_eq!(pick(lgpl, &H264_ENCODERS), Some("libopenh264"));
    }

    /// `libx264rgb`'s description contains `libx264`, so a substring match would
    /// find it in a build that has only the RGB variant.
    #[test]
    fn does_not_match_an_encoder_named_in_a_description() {
        let only_rgb = " V....D libx264rgb   libx264 H.264 / AVC / MPEG-4 AVC RGB (codec h264)\n";
        assert_eq!(pick(only_rgb, &H264_ENCODERS), None);
    }

    #[test]
    fn a_build_with_no_h264_is_refused() {
        assert_eq!(pick(" A....D aac   AAC\n", &H264_ENCODERS), None);
    }

    /// A configured path that does not exist must be reported, not skipped:
    /// falling through to some other ffmpeg leaves the wrong setting on screen
    /// with nothing to say it is being ignored.
    #[test]
    fn a_wrong_configured_path_is_an_error_rather_than_a_fallback() {
        let error = locate("Z:\\nothing\\here\\ffmpeg.exe").unwrap_err();
        assert!(error.contains("Preferences"), "{error}");
    }
}
