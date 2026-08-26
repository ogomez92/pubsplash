//! Publishing to an RTMP ingest by driving ffmpeg, and the reason the video
//! track exists.
//!
//! # The shape of the problem
//!
//! Everywhere else in Pubsplash the outgoing audio is MP3 bytes and the
//! transport is a socket the app owns. Here it is neither. YouTube's RTMP ingest
//! takes **H.264 video and AAC audio in FLV**, and it takes the video whether or
//! not the broadcast is about pictures: an audio-only source is refused. So a
//! radio show has to carry a still frame, and the app has to produce two codecs
//! and a container it has none of.
//!
//! ffmpeg does all of it, and is driven as a child process rather than linked
//! as libraries. That is not only about the codecs: a plugin-grade crash inside
//! an encoder would take the app down mid-broadcast, and here it takes down a
//! process the supervisor below simply starts again.
//!
//! # Two inputs, one clock — and **no `-re`**
//!
//! Audio arrives on the child's stdin as raw interleaved 16-bit PCM at the
//! mixer's own rate, which is to say in real time: the mixer produces a block
//! every 10 ms and nothing here speeds that up. **That pipe is the only clock
//! this pipeline needs, and it is the whole reason `-re` must not appear.**
//!
//! The obvious-looking thing to write is `-re` on the picture input, since a
//! still image or an `lavfi` colour source will otherwise generate frames as
//! fast as it is asked to. Every "still image plus audio to RTMP" recipe on the
//! internet says so, and they are right for the case they are written for — an
//! audio *file*, which has no natural pace of its own.
//!
//! Here it is wrong, and expensively so. ffmpeg pulls picture frames only as
//! fast as the muxer needs them against the audio timeline, so the pipe already
//! holds the picture to `-r`. Adding `-re` puts a *second* throttle on the same
//! stream, in the input thread, and the two compound: measured against a
//! real-time PCM feed, the identical command runs at **0.49–0.59× real time**
//! with `-re` and **1.01×** without it, both producing exactly the 2 fps asked
//! for. Below real time, the pipe backs up, ffmpeg's input queue fills, it stops
//! reading stdin, and [`RtmpProcess::send`] times out — which reads as a dropped
//! connection and restarts the whole thing, every few seconds, forever. Do not
//! put `-re` back; [`tests::never_paces_the_inputs`] is there to stop it.
//!
//! # What must never be logged
//!
//! The destination URL has the stream key as its last path segment, and the
//! stream key is a password: anyone holding it can broadcast to the channel.
//! [`RtmpTarget::args`] is therefore the *only* thing that ever builds the full
//! URL, and [`RtmpTarget::describe`] is what goes in the log. ffmpeg's own
//! stderr is forwarded to the log, and ffmpeg prints the destination on error —
//! so [`redact`] runs over every line before it gets there.

use crate::secret::Secret;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin};
use tokio::time::timeout;

/// Bounds one write into the child's stdin.
///
/// The same job [`super::icecast::WRITE_TIMEOUT`] does, one layer along: ffmpeg
/// stops reading stdin when its own output is blocked, so a wedged RTMP session
/// shows up here as a write that never returns. Without a bound, the UI would
/// claim to be live for as long as the pipe buffer plus ffmpeg's internal queues
/// took to fill — which is minutes.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the child is given to finish its FLV stream after stdin closes.
///
/// `-shortest` should end it as soon as the audio input does, but the loop over
/// a still image has been known to outlive that, and a stream that will not stop
/// holds up `NetHandle::drop` — which joins the network thread on app exit.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// How many lines of ffmpeg's stderr are kept to explain a failure.
///
/// ffmpeg says why it is dying on the way out, but the useful line is rarely the
/// last one — a failed RTMP handshake prints the error, then a summary, then
/// "Conversion failed". Keeping a handful and reporting them together is what
/// makes the log say `Connection refused` rather than `Conversion failed`.
const STDERR_LINES: usize = 12;

/// Video frame rate.
///
/// Two frames a second, not one: YouTube wants a keyframe every few seconds and
/// measures the interval in frames, so a 1 fps stream makes every keyframe
/// setting a whole number of *seconds* apart with no room to tune. It is also
/// the floor at which some ingests stop counting the stream as having video at
/// all. Nothing moves in the picture, so the cost of the second frame is a few
/// hundred bytes a second.
const FRAME_RATE: u32 = 2;

/// Keyframe interval, in frames — so [`FRAME_RATE`] of these is one second.
///
/// YouTube requires a keyframe at least every four seconds and recommends two.
/// At 2 fps that is this many frames.
const KEYFRAME_INTERVAL: u32 = FRAME_RATE * 2;

/// The picture size sent when the user has chosen an image. 720p is the largest
/// size YouTube will not transcode down for most viewers, and a still frame
/// costs nothing extra to send at it.
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

/// The colour of the generated frame when no image is configured. Dark, because
/// it is what a viewer stares at for the length of the show.
const DEFAULT_COLOUR: &str = "0x101418";

#[derive(Debug)]
pub enum RtmpError {
    /// The child could not be started at all — usually a path that stopped being
    /// an executable between Connect and Start.
    Launch(String),
    /// ffmpeg exited. `detail` is the tail of its stderr.
    Exited { status: String, detail: String },
    /// A write into the child took longer than [`WRITE_TIMEOUT`].
    Stalled,
    Io(String),
}

impl std::fmt::Display for RtmpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.explain())
    }
}

impl RtmpError {
    /// Whether trying again could plausibly work.
    ///
    /// Only a launch failure is terminal. Everything else — a refused
    /// connection, a dropped session, a stall — is the ordinary shape of a
    /// network outage, and the retry budget rather than this decides when to
    /// stop. A rejected stream key is deliberately *not* singled out: RTMP
    /// carries no status code, so the only evidence is prose in ffmpeg's stderr,
    /// and matching on that would turn a wording change into a stream that
    /// retries for four minutes against a server that will never accept it —
    /// or, far worse the other way, a recoverable blip treated as fatal.
    pub fn retryable(&self) -> bool {
        !matches!(self, RtmpError::Launch(_))
    }

    pub fn explain(&self) -> String {
        match self {
            RtmpError::Launch(what) => format!("FFmpeg could not be started: {what}"),
            RtmpError::Exited { status, detail } if detail.is_empty() => {
                format!("FFmpeg stopped ({status})")
            }
            RtmpError::Exited { status, detail } => {
                format!("FFmpeg stopped ({status}): {detail}")
            }
            RtmpError::Stalled => format!(
                "FFmpeg stopped reading audio for {} seconds",
                WRITE_TIMEOUT.as_secs()
            ),
            RtmpError::Io(what) => format!("the connection to FFmpeg failed: {what}"),
        }
    }
}

/// Everything one publish needs. Snapshotted by the UI before it crosses to the
/// network thread.
#[derive(Debug, Clone)]
pub struct RtmpTarget {
    pub ffmpeg: PathBuf,
    /// The application URL, without the stream key.
    pub url: String,
    pub key: Secret,
    /// The still image. `None` generates a plain frame instead, so a service
    /// works before the user has chosen anything.
    pub image: Option<PathBuf>,
    pub audio_bitrate_kbps: u32,
    pub video_bitrate_kbps: u32,
    /// From [`crate::ffmpeg::Capabilities`], so whichever build the user has is
    /// asked for an encoder it actually carries.
    pub h264: &'static str,
    pub aac: &'static str,
}

impl RtmpTarget {
    /// The full destination, key included. Never log this.
    fn destination(&self) -> String {
        format!(
            "{}/{}",
            self.url.trim().trim_end_matches('/'),
            self.key.as_str().trim().trim_start_matches('/')
        )
    }

    /// The destination as it may appear in a log: the application URL, and the
    /// fact that a key was appended, but never the key.
    pub fn describe(&self) -> String {
        format!("{}/<stream key>", self.url.trim().trim_end_matches('/'))
    }

    /// The whole command line, destination last.
    ///
    /// Built as one function so the ordering rules stay in one place. Input
    /// options must precede the `-i` they belong to, which is why `-re` and
    /// `-loop` appear twice over and cannot be hoisted.
    pub fn args(&self) -> Vec<String> {
        /// Turns a run of fixed flags into owned strings.
        ///
        /// A free function rather than a closure over `args`: a closure holding
        /// the vector borrowed would rule out the `args.push` calls the
        /// computed values need, and interleaving the two is the whole shape of
        /// a command line.
        fn fixed<'a>(values: &'a [&'a str]) -> impl Iterator<Item = String> + 'a {
            values.iter().map(|value| value.to_string())
        }

        let mut args: Vec<String> = Vec::new();
        let push = |args: &mut Vec<String>, values: &[&str]| args.extend(fixed(values));

        push(&mut args, &["-hide_banner"]);
        // Without this, ffmpeg treats the inherited console as an interactive
        // terminal and can consume keystrokes; with no console at all (a release
        // build is `windows_subsystem = "windows"`) it has been seen to exit at
        // once on a stdin read it should never have made.
        push(&mut args, &["-nostdin"]);
        push(&mut args, &["-loglevel", "warning"]);

        // Input 0: our PCM. `thread_queue_size` is raised because the default of
        // 8 packets is a tenth of a second here, and ffmpeg drops input packets
        // rather than blocking when it overflows — which is a hole in the audio
        // with only a warning to show for it.
        push(&mut args, &["-thread_queue_size", "1024"]);
        push(&mut args, &["-f", "s16le"]);
        push(&mut args, &["-ar", "48000"]);
        push(&mut args, &["-ac", "2"]);
        push(&mut args, &["-i", "pipe:0"]);

        // Input 1: the picture. Note the absence of `-re` — the audio pipe above
        // is the clock, and a second throttle here costs half of real time. The
        // module header has the measurements.
        push(&mut args, &["-thread_queue_size", "512"]);
        match &self.image {
            Some(path) => {
                push(&mut args, &["-loop", "1"]);
                args.push("-framerate".into());
                args.push(FRAME_RATE.to_string());
                args.push("-i".into());
                args.push(path.to_string_lossy().into_owned());
            }
            None => {
                push(&mut args, &["-f", "lavfi"]);
                args.push("-i".into());
                args.push(format!(
                    "color=c={DEFAULT_COLOUR}:s={WIDTH}x{HEIGHT}:r={FRAME_RATE}"
                ));
            }
        }

        // Explicit, because ffmpeg's default stream selection would pick the
        // *best* audio and video across both inputs and there is no reason to
        // leave that to a heuristic.
        push(&mut args, &["-map", "1:v:0"]);
        push(&mut args, &["-map", "0:a:0"]);

        args.push("-c:v".into());
        args.push(self.h264.to_string());
        // libx264's knobs, and only its: libopenh264 refuses an unknown private
        // option outright, so offering them to it would fail the launch.
        if self.h264 == "libx264" {
            push(&mut args, &["-preset", "veryfast"]);
            // Tells the rate control the picture is static, which is exactly
            // what it is.
            push(&mut args, &["-tune", "stillimage"]);
            push(&mut args, &["-profile:v", "main"]);
        }
        push(&mut args, &["-pix_fmt", "yuv420p"]);
        args.push("-r".into());
        args.push(FRAME_RATE.to_string());
        args.push("-g".into());
        args.push(KEYFRAME_INTERVAL.to_string());
        args.push("-keyint_min".into());
        args.push(KEYFRAME_INTERVAL.to_string());
        args.push("-b:v".into());
        args.push(format!("{}k", self.video_bitrate_kbps));
        args.push("-maxrate".into());
        args.push(format!("{}k", self.video_bitrate_kbps));
        args.push("-bufsize".into());
        args.push(format!("{}k", self.video_bitrate_kbps.saturating_mul(2)));

        // A user's image is any size and any aspect; the pad centres it on a
        // 720p frame rather than stretching it, and the even dimensions are what
        // `yuv420p` requires. The generated colour source is already the right
        // size, so it needs none of this.
        if self.image.is_some() {
            args.push("-vf".into());
            args.push(format!(
                "scale={WIDTH}:{HEIGHT}:force_original_aspect_ratio=decrease,\
                 pad={WIDTH}:{HEIGHT}:(ow-iw)/2:(oh-ih)/2:color=black"
            ));
        }

        args.push("-c:a".into());
        args.push(self.aac.to_string());
        args.push("-b:a".into());
        args.push(format!("{}k", self.audio_bitrate_kbps));
        // Left at the mixer's rate rather than resampled to 44.1: YouTube takes
        // 48 kHz, and a resample here would be a pointless pass over every
        // sample of the broadcast.
        push(&mut args, &["-ar", "48000"]);
        push(&mut args, &["-ac", "2"]);

        // Ends the run when the audio input ends, which is how stopping a stream
        // reaches ffmpeg: the pipe closes. Without it the looping picture would
        // keep the process alive forever.
        push(&mut args, &["-shortest"]);
        push(&mut args, &["-f", "flv"]);
        // The FLV muxer finishes by seeking back to the header to write the
        // duration and file size, which a socket cannot do. Without this it
        // fails twice on the way out of *every* stream and says so on stderr,
        // and this module forwards stderr to the log — so a clean shutdown
        // would end with two warnings that look like a fault and are not.
        push(&mut args, &["-flvflags", "no_duration_filesize"]);
        args.push(self.destination());
        args
    }
}

/// Replaces anything that looks like a stream key with a placeholder.
///
/// ffmpeg echoes the destination URL in most of its connection errors, and those
/// go straight to the log the user is asked to attach to a bug report. Matching
/// on the URL rather than on the key means a key that appears for any other
/// reason is caught too.
fn redact(line: &str, url: &str) -> String {
    let prefix = url.trim().trim_end_matches('/');
    if prefix.is_empty() {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(prefix) {
        out.push_str(&rest[..at]);
        out.push_str(prefix);
        out.push_str("/<stream key>");
        rest = &rest[at + prefix.len()..];
        // Drop the key itself: everything up to the next character that cannot
        // be in a URL path segment.
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '\'' || c == '"')
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// A running ffmpeg, publishing.
pub struct RtmpProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    /// The tail of stderr, shared with the reader task.
    recent: Arc<Mutex<VecDeque<String>>>,
}

impl RtmpProcess {
    /// Starts ffmpeg and returns as soon as the process exists.
    ///
    /// Deliberately does **not** wait to see whether the RTMP handshake
    /// succeeded. ffmpeg does not say so — there is no "connected" line at
    /// `warning` level — and the only evidence either way is whether it is still
    /// running a moment later, which is what the supervisor watches for. Waiting
    /// here would mean holding up `Start streaming` on a guess.
    pub async fn start(target: &RtmpTarget) -> Result<Self, RtmpError> {
        let mut command = tokio::process::Command::new(&target.ffmpeg);
        command
            .args(target.args())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            // See `ffmpeg::command`: without this a console window appears and
            // takes focus, which for a screen-reader user mid-broadcast means
            // the focus they were using is simply gone.
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        // The child must not outlive us silently: `kill_on_drop` covers the
        // paths where the task is aborted rather than shut down, which is what
        // `ActiveStream::abort` does.
        command.kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|e| RtmpError::Launch(format!("{} ({e})", target.ffmpeg.display())))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RtmpError::Launch("no pipe to FFmpeg's input".to_string()))?;

        let recent = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_LINES)));
        if let Some(stderr) = child.stderr.take() {
            spawn_stderr_reader(stderr, recent.clone(), target.url.clone());
        }

        Ok(Self {
            child,
            stdin: Some(stdin),
            recent,
        })
    }

    /// Whether the child has exited, and with what.
    ///
    /// Non-blocking, and checked every time a send succeeds: a dropped RTMP
    /// session makes ffmpeg exit, and writing into the pipe of a dead process
    /// can succeed for as long as the pipe buffer has room — so the write alone
    /// is not enough to notice.
    pub fn exited(&mut self) -> Option<RtmpError> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(RtmpError::Exited {
                status: describe_status(status),
                detail: self.detail(),
            }),
            Ok(None) => None,
            Err(e) => Some(RtmpError::Io(e.to_string())),
        }
    }

    /// The tail of ffmpeg's stderr, joined for one log line.
    fn detail(&self) -> String {
        self.recent
            .lock()
            .map(|lines| {
                lines
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default()
    }

    /// Writes one block of PCM.
    ///
    /// Loops over `write` rather than calling `write_all`, so the timeout can be
    /// applied per call with a byte count we still know. A cancelled `write_all`
    /// does not report how much it wrote, and a resumed stream that is off by an
    /// odd number of bytes has its channels swapped for the rest of the
    /// broadcast. Since a stall is treated as fatal to this child anyway the
    /// offset is never actually resumed — but the alternative silently *could*
    /// be, which is the kind of bug that survives for years.
    pub async fn send(&mut self, data: &[u8]) -> Result<(), RtmpError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(RtmpError::Io("FFmpeg's input is closed".to_string()));
        };
        let mut written = 0usize;
        while written < data.len() {
            match timeout(WRITE_TIMEOUT, stdin.write(&data[written..])).await {
                Ok(Ok(0)) => {
                    return Err(RtmpError::Exited {
                        status: "input closed".to_string(),
                        detail: self.detail(),
                    });
                }
                Ok(Ok(count)) => written += count,
                Ok(Err(e)) => return Err(RtmpError::Io(e.to_string())),
                Err(_) => return Err(RtmpError::Stalled),
            }
        }
        Ok(())
    }

    /// Ends the broadcast tidily: close the input, let ffmpeg flush its FLV, and
    /// kill it if it will not go.
    pub async fn close(mut self) {
        // Dropping stdin is what `-shortest` is waiting for.
        self.stdin = None;
        match timeout(SHUTDOWN_GRACE, self.child.wait()).await {
            Ok(Ok(status)) => log::info!("FFmpeg finished ({})", describe_status(status)),
            Ok(Err(e)) => log::warn!("FFmpeg could not be waited for: {e}"),
            Err(_) => {
                log::warn!(
                    "FFmpeg did not finish within {}s; stopping it",
                    SHUTDOWN_GRACE.as_secs()
                );
                let _ = self.child.kill().await;
            }
        }
    }

    /// Stops the child now, for the reconnect path where its output is already
    /// gone and there is nothing worth flushing.
    pub async fn abandon(mut self) {
        self.stdin = None;
        let _ = self.child.kill().await;
    }
}

fn describe_status(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(0) => "exit code 0".to_string(),
        Some(code) => format!("exit code {code}"),
        None => "stopped".to_string(),
    }
}

/// Forwards ffmpeg's stderr to the log and keeps the tail for [`RtmpProcess::detail`].
fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    recent: Arc<Mutex<VecDeque<String>>>,
    url: String,
) {
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = redact(line.trim(), &url);
            if line.is_empty() {
                continue;
            }
            // At `-loglevel warning` ffmpeg is silent unless something is wrong,
            // so every line here is worth the log — and this is the only place
            // that ever says why a YouTube stream failed.
            log::warn!("FFmpeg: {line}");
            if let Ok(mut recent) = recent.lock() {
                if recent.len() == STDERR_LINES {
                    recent.pop_front();
                }
                recent.push_back(line);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> RtmpTarget {
        RtmpTarget {
            ffmpeg: PathBuf::from("ffmpeg.exe"),
            url: "rtmps://a.rtmps.youtube.com/live2".into(),
            key: Secret::new("abcd-efgh-ijkl"),
            image: None,
            audio_bitrate_kbps: 128,
            video_bitrate_kbps: 400,
            h264: "libx264",
            aac: "aac",
        }
    }

    /// A trailing slash on the URL and a leading one on the key must not produce
    /// a doubled separator: RTMP servers answer that with a mount that does not
    /// exist, which looks exactly like a wrong key.
    #[test]
    fn joins_the_url_and_key_with_exactly_one_slash() {
        let mut t = target();
        t.url = "rtmp://a.rtmp.youtube.com/live2/".into();
        t.key = Secret::new("/abcd");
        assert_eq!(t.destination(), "rtmp://a.rtmp.youtube.com/live2/abcd");
    }

    /// ffmpeg takes the output URL as a bare positional argument, so it has to
    /// be last: anything after it would be read as another output.
    #[test]
    fn the_destination_is_last_and_carries_the_key() {
        let args = target().args();
        assert_eq!(
            args.last().unwrap(),
            "rtmps://a.rtmps.youtube.com/live2/abcd-efgh-ijkl"
        );
        assert!(args.iter().any(|a| a == "flv"), "{args:?}");
    }

    /// The key is a password. Nothing that reaches a log may carry it.
    #[test]
    fn the_log_description_never_carries_the_key() {
        let described = target().describe();
        assert!(!described.contains("abcd-efgh-ijkl"), "{described}");
        assert_eq!(described, "rtmps://a.rtmps.youtube.com/live2/<stream key>");
    }

    /// ffmpeg echoes the destination in its errors, and those go to the log.
    #[test]
    fn redacts_the_key_out_of_ffmpeg_output() {
        let url = "rtmps://a.rtmps.youtube.com/live2";
        let line = "[flv @ 000] rtmps://a.rtmps.youtube.com/live2/abcd-efgh-ijkl: Connection refused";
        let clean = redact(line, url);
        assert!(!clean.contains("abcd-efgh-ijkl"), "{clean}");
        assert!(clean.contains("<stream key>"), "{clean}");
        assert!(clean.contains("Connection refused"), "{clean}");
    }

    /// Two mentions in one line, which ffmpeg does produce when it retries.
    #[test]
    fn redacts_every_occurrence() {
        let url = "rtmp://x/live";
        let clean = redact("rtmp://x/live/KEY1 failed, retrying rtmp://x/live/KEY1", url);
        assert!(!clean.contains("KEY1"), "{clean}");
        assert_eq!(clean.matches("<stream key>").count(), 2, "{clean}");
    }

    /// `-re` is the one option that must never appear here, on either input.
    ///
    /// It reads as the obviously-correct thing to add — it is in every recipe
    /// for sending a still image with audio — and it costs half of real time,
    /// because the live PCM pipe is already pacing the pipeline and the two
    /// throttles compound. Measured: 0.49–0.59× with it, 1.01× without. Below
    /// real time the pipe backs up until ffmpeg stops reading it, which the
    /// sender sees as a dropped connection and answers by restarting, forever.
    /// See the module header.
    #[test]
    fn never_paces_the_inputs() {
        for image in [None, Some(PathBuf::from("C:\\art\\cover.png"))] {
            let mut t = target();
            t.image = image;
            let args = t.args();
            assert!(
                !args.iter().any(|a| a == "-re"),
                "-re must not appear: it halves throughput here. {args:?}"
            );
        }
    }

    /// The picture is still asked for at a fixed rate; without `-re` that is the
    /// output `-r` and nothing else.
    #[test]
    fn asks_for_a_fixed_picture_rate() {
        let args = target().args();
        let rate = args
            .iter()
            .position(|a| a == "-r")
            .map(|at| args[at + 1].clone())
            .expect("an output frame rate");
        assert_eq!(rate, FRAME_RATE.to_string());
    }

    /// libopenh264 refuses libx264's private options, so offering them to it
    /// fails the launch outright.
    #[test]
    fn offers_x264_only_options_to_x264() {
        let mut t = target();
        t.h264 = "libopenh264";
        let args = t.args();
        assert!(args.contains(&"libopenh264".to_string()));
        for x264_only in ["-preset", "-tune", "-profile:v"] {
            assert!(
                !args.iter().any(|a| a == x264_only),
                "{x264_only} must not be offered to libopenh264: {args:?}"
            );
        }
        assert!(target().args().iter().any(|a| a == "-tune"));
    }

    /// A user's image is scaled and padded; the generated frame is already the
    /// right size and must not be put through a filter that would only cost CPU.
    #[test]
    fn filters_a_chosen_image_and_not_the_generated_one() {
        assert!(!target().args().iter().any(|a| a == "-vf"));
        let mut t = target();
        t.image = Some(PathBuf::from("C:\\art\\cover.png"));
        let args = t.args();
        let filter = args
            .iter()
            .position(|a| a == "-vf")
            .map(|at| args[at + 1].clone())
            .expect("an image is filtered");
        assert!(filter.contains("scale=1280:720"), "{filter}");
        assert!(filter.contains("pad=1280:720"), "{filter}");
        assert!(args.iter().any(|a| a == "C:\\art\\cover.png"));
        assert!(args.iter().any(|a| a == "-loop"));
    }

    /// `-shortest` is how stopping a stream reaches ffmpeg: closing stdin ends
    /// the audio input, which ends the run. Without it the looping picture keeps
    /// the process alive after the broadcast is over.
    #[test]
    fn stops_when_the_audio_ends() {
        assert!(target().args().iter().any(|a| a == "-shortest"));
    }

    /// YouTube requires a keyframe every few seconds, counted in frames.
    #[test]
    fn sends_a_keyframe_every_two_seconds() {
        let args = target().args();
        let gop = args
            .iter()
            .position(|a| a == "-g")
            .map(|at| args[at + 1].clone())
            .unwrap();
        assert_eq!(gop, (FRAME_RATE * 2).to_string());
    }

    /// Only a launch failure is worth giving up on at once; everything else is
    /// an outage the budget decides about.
    #[test]
    fn only_a_launch_failure_is_terminal() {
        assert!(!RtmpError::Launch("no such file".into()).retryable());
        assert!(RtmpError::Stalled.retryable());
        assert!(
            RtmpError::Exited {
                status: "exit code 1".into(),
                detail: String::new()
            }
            .retryable()
        );
    }
}
