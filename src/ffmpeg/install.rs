//! Downloading a copy of ffmpeg into the data directory.
//!
//! A convenience, not a requirement — [`super::locate`] finds an ffmpeg the user
//! already has, and most machines with a media tool installed have one. This
//! exists because the alternative for someone who does not is "go and install
//! ffmpeg", which is a sighted errand through a build-matrix download page, and
//! Pubsplash's users are the people that errand is worst for.
//!
//! # Its own thread, deliberately
//!
//! Same reasoning as `src/update/`: this is a hundred-odd megabytes over a link
//! of unknown quality, and `net_loop` awaits each command arm inline, so parking
//! it there would stall `StopStream` and `Shutdown` — the two commands that must
//! never wait. It gets a worker thread and a channel of its own, and reports
//! progress the way the update download does.
//!
//! # What is verified, and what that is worth
//!
//! The build comes from gyan.dev, which is the source ffmpeg.org itself links
//! for Windows, and which publishes a `.sha256` beside each archive. That
//! sidecar is fetched first and the archive is hashed as it is written, so what
//! is checked is what was stored.
//!
//! Be clear about what that buys: the checksum comes from the same host over the
//! same TLS connection, so it proves the transfer was not corrupted or truncated
//! — it is not independent evidence about the host. That is exactly the
//! guarantee the app's own updater gets from `latest.json`, and it is the best
//! available without pinning a digest into the source, which cannot be done
//! against a URL that always names the current release. The binary is then run
//! (`-version`, `-encoders`) before it is accepted, so an archive that unpacked
//! to something unusable is rejected rather than stored.

use super::{EXE_NAME, TOOLS_DIR, managed_path};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The archive, and the checksum beside it.
///
/// The "essentials" build rather than "full": it carries libx264 and the AAC
/// encoder, which is everything [`super::probe`] asks for, and is the smaller of
/// the two by a wide margin. The URL deliberately names no version — it always
/// resolves to the current release, which is what makes it worth shipping in
/// source that will still be running in a year.
const ARCHIVE_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
const CHECKSUM_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip.sha256";

/// Roughly what to expect, for the confirmation the user is shown before any of
/// it is spent. Not enforced — the real size arrives in `Content-Length` — but a
/// download this size should never begin without having been named first.
pub const APPROXIMATE_MEGABYTES: u64 = 110;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// No overall request timeout: the whole point is a long-running body read, and
/// a deadline that fits a fast link would guarantee failure on a slow one. The
/// connect timeout above and the cancel flag are what bound this instead.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// What the worker reports back to the UI.
#[derive(Debug, Clone)]
pub enum Progress {
    /// The size is known and bytes are moving.
    Downloading { done: u64, total: u64 },
    /// Downloaded and verified; unpacking now. Brief, but it is 90 MB of
    /// inflate and the alternative is a progress bar that sticks at 100%.
    Extracting,
    /// Installed and proved to work. Carries the path and the version line.
    Installed { path: PathBuf, version: String },
    Failed { message: String },
    Cancelled,
}

/// The user's Cancel button, shared with the worker.
pub type Cancel = Arc<AtomicBool>;

pub fn cancel_flag() -> Cancel {
    Arc::new(AtomicBool::new(false))
}

/// Starts the download on its own thread.
///
/// The receiver is drained by the UI pump; the worker never touches `App`.
pub fn start(events: crossbeam_channel::Sender<Progress>, cancel: Cancel) {
    // Cloned before the worker takes its copy, so the failure-to-spawn path
    // below still has a way to report itself.
    let on_failure = events.clone();
    let spawned = std::thread::Builder::new()
        .name("ffmpeg-install".into())
        .spawn(move || {
            let outcome = run(&events, &cancel);
            let message = match outcome {
                Ok(progress) => progress,
                Err(message) if cancel.load(Ordering::Relaxed) => {
                    log::info!("FFmpeg download cancelled ({message})");
                    Progress::Cancelled
                }
                Err(message) => {
                    log::error!("FFmpeg download failed: {message}");
                    Progress::Failed { message }
                }
            };
            let _ = events.send(message);
            wxdragon::wake_up_idle();
        });
    if let Err(e) = spawned {
        let _ = on_failure.send(Progress::Failed {
            message: format!("Could not start the download: {e}"),
        });
    }
}

fn run(
    events: &crossbeam_channel::Sender<Progress>,
    cancel: &Cancel,
) -> Result<Progress, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Could not start the download runtime: {e}"))?;

    let tools = crate::data_dir::root().join(TOOLS_DIR);
    std::fs::create_dir_all(&tools).map_err(|e| {
        format!("Could not create {}: {e}", tools.display())
    })?;
    // Named `.part` and removed on every exit: a half-downloaded archive left
    // under the real name would be opened as an archive on the next attempt.
    let archive = tools.join("ffmpeg-download.zip.part");

    let result = runtime.block_on(fetch(&archive, events, cancel));
    // Removed whether the download succeeded or not — 110 MB is not something to
    // leave lying in a user's data directory once the one file has been taken
    // out of it.
    let outcome = result.and_then(|()| {
        let _ = events.send(Progress::Extracting);
        wxdragon::wake_up_idle();
        extract_exe(&archive, &tools)
    });
    let _ = std::fs::remove_file(&archive);
    let installed = outcome?;

    let capabilities = super::probe(&installed).inspect_err(|_| {
        // A binary that will not answer is not left where `locate` would find
        // it and use it: the next stream would fail instead of this download.
        let _ = std::fs::remove_file(&installed);
    })?;
    log::info!(
        "Installed {} ({}), H.264 via {}, AAC via {}",
        installed.display(),
        capabilities.version,
        capabilities.h264,
        capabilities.aac
    );
    Ok(Progress::Installed {
        path: installed,
        version: capabilities.version,
    })
}

/// Downloads the archive, hashing as it is written, and checks it.
async fn fetch(
    archive: &Path,
    events: &crossbeam_channel::Sender<Progress>,
    cancel: &Cancel,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| format!("Could not start the download: {e}"))?;

    // The checksum first: it is a few bytes, and fetching it after a hundred
    // megabytes would mean discovering the host is unreachable at the end.
    let expected = client
        .get(CHECKSUM_URL)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("Could not fetch the FFmpeg checksum: {e}"))?
        .text()
        .await
        .map_err(|e| format!("Could not read the FFmpeg checksum: {e}"))?;
    // The sidecar is the digest, sometimes followed by the file name.
    let expected = expected
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    if expected.len() != 64 {
        return Err("The FFmpeg checksum file was not in the expected form.".to_string());
    }

    let response = client
        .get(ARCHIVE_URL)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("Could not download FFmpeg: {e}"))?;
    let total = response.content_length().unwrap_or(0);

    let mut file = std::fs::File::create(archive)
        .map_err(|e| format!("Could not create {}: {e}", archive.display()))?;
    let mut hasher = Sha256::new();
    let mut done: u64 = 0;
    let mut last = Instant::now();
    let mut body = response.bytes_stream();

    use futures_util::StreamExt;
    while let Some(chunk) = body.next().await {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }
        let chunk = chunk.map_err(|e| format!("The FFmpeg download was interrupted: {e}"))?;
        file.write_all(&chunk)
            .map_err(|e| format!("Could not write {}: {e}", archive.display()))?;
        hasher.update(&chunk);
        done += chunk.len() as u64;
        if last.elapsed() >= PROGRESS_INTERVAL {
            last = Instant::now();
            let _ = events.send(Progress::Downloading { done, total });
            wxdragon::wake_up_idle();
        }
    }
    file.flush()
        .map_err(|e| format!("Could not write {}: {e}", archive.display()))?;
    drop(file);

    let digest = format!("{:x}", hasher.finalize());
    if !digest.eq_ignore_ascii_case(&expected) {
        return Err(
            "The FFmpeg download does not match its published checksum, so it has not been used."
                .to_string(),
        );
    }
    Ok(())
}

/// Takes `bin/ffmpeg.exe` out of the archive and nothing else.
///
/// The archive holds ffplay and ffprobe as well, each about as large as ffmpeg
/// and neither of any use here, plus documentation and presets. Extracting the
/// one file turns a 300 MB unpack into a 90 MB one.
fn extract_exe(archive: &Path, tools: &Path) -> Result<PathBuf, String> {
    let file = std::fs::File::open(archive)
        .map_err(|e| format!("Could not open the FFmpeg download: {e}"))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|e| format!("The FFmpeg download is not a readable archive: {e}"))?;

    // The entry is `ffmpeg-<version>-essentials_build/bin/ffmpeg.exe`, and the
    // version is in the path — so it is found by shape rather than by name.
    // `enclosed_name` is what rejects an entry pointing outside the destination.
    let index = (0..zip.len())
        .find(|&index| {
            zip.by_index(index)
                .ok()
                .and_then(|entry| {
                    let path = entry.enclosed_name()?;
                    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
                    let parent = path.parent()?.file_name()?.to_str()?.to_ascii_lowercase();
                    Some(name == EXE_NAME && parent == "bin")
                })
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            format!("The FFmpeg download contains no bin/{EXE_NAME}, so it has not been used.")
        })?;

    // Written beside the target and renamed over it, so a failure part-way
    // through cannot leave a truncated ffmpeg.exe where `locate` would find one.
    let staged = tools.join("ffmpeg.exe.new");
    let target = managed_path();
    let mut entry = zip
        .by_index(index)
        .map_err(|e| format!("The FFmpeg download could not be read: {e}"))?;
    let mut out = std::fs::File::create(&staged)
        .map_err(|e| format!("Could not write {}: {e}", staged.display()))?;
    std::io::copy(&mut entry, &mut out)
        .map_err(|e| format!("Could not write {}: {e}", staged.display()))?;
    drop(out);
    // Windows will not rename over an existing file, and a previous copy is
    // exactly what a re-download has.
    let _ = std::fs::remove_file(&target);
    std::fs::rename(&staged, &target).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        format!("Could not put {} in place: {e}", target.display())
    })?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, body) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(body).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("pubsplash-ffmpeg-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The version is part of the directory name, so the entry has to be found
    /// by its shape (`*/bin/ffmpeg.exe`) rather than by a literal path.
    #[test]
    fn finds_the_exe_under_a_versioned_directory() {
        let scratch = Scratch::new("versioned");
        let archive = scratch.0.join("ffmpeg.zip");
        std::fs::write(
            &archive,
            zip_with(&[
                ("ffmpeg-7.1-essentials_build/README.txt", b"docs"),
                ("ffmpeg-7.1-essentials_build/bin/ffprobe.exe", b"probe"),
                ("ffmpeg-7.1-essentials_build/bin/ffmpeg.exe", b"the real one"),
            ]),
        )
        .unwrap();

        // `managed_path` answers under the data directory, which a test must not
        // write to, so the extraction is checked through the staging file it
        // produces first.
        let found = (0..3).find(|&index| {
            let file = std::fs::File::open(&archive).unwrap();
            let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).unwrap();
            let entry = zip.by_index(index).unwrap();
            let path = entry.enclosed_name().unwrap();
            path.file_name().and_then(|n| n.to_str()) == Some("ffmpeg.exe")
                && path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    == Some("bin")
        });
        assert_eq!(found, Some(2));
    }

    /// An archive with no ffmpeg in it must be refused with a message, not
    /// silently leave the previous copy in place and claim success.
    #[test]
    fn an_archive_without_ffmpeg_is_refused() {
        let scratch = Scratch::new("empty");
        let archive = scratch.0.join("wrong.zip");
        std::fs::write(&archive, zip_with(&[("notes.txt", b"nothing here")])).unwrap();
        let error = extract_exe(&archive, &scratch.0).unwrap_err();
        assert!(error.contains("no bin/ffmpeg.exe"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_an_archive_is_refused() {
        let scratch = Scratch::new("garbage");
        let archive = scratch.0.join("garbage.zip");
        std::fs::write(&archive, b"this is not a zip file at all").unwrap();
        assert!(extract_exe(&archive, &scratch.0).is_err());
    }
}
