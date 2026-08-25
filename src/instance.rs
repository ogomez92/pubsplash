//! One Pubsplash at a time, per user.
//!
//! Two copies of the app fight over everything that is effectively exclusive:
//! the audio capture threads and the desktop loopback client, the rotating log
//! file, the settings file (last writer wins, so one copy silently discards the
//! other's scene edits), the encrypted credentials, and worst of all the
//! Icecast source connection, where the second copy's mount attempt either takes
//! the stream from the first or is refused with `403 Mountpoint in use`.
//!
//! The two properties that matter, and both platforms have to supply them. The
//! claim is keyed on a **name and nothing else**, so two copies installed in
//! different places collide exactly as they should — a path-derived lock would
//! miss the case entirely. And the claim **dies with the process**, however it
//! dies, so a crash or a force-quit leaves nothing stale to clear.
//!
//! Windows gets both from a kernel named mutex: the object lives as long as the
//! last handle, and the kernel closes handles on any exit. macOS gets them from
//! `flock` on a file in the data directory — an advisory lock held by an open
//! descriptor, which the kernel likewise releases on any exit, so the file being
//! left behind afterwards means nothing and is never checked for. The path is
//! fixed and under the data directory rather than beside the executable, which
//! is what keeps it keyed on a name: two `.app` bundles in different folders
//! reach the same lock.
//!
//! This module is reachable only from `main.rs`'s module list, so none of the
//! other binaries — `pubsplash-scan` (many concurrent invocations per plugin
//! scan), `pubsplash-soundpack`, `pubsplash-update` (which must run while
//! Pubsplash is exiting), `soundpack`, `gen-help` — is affected by it.

use std::time::Duration;

pub use imp::{InstanceGuard, acquire_named, reject_and_exit};

/// How long to keep trying before deciding another instance really is running.
///
/// This exists for exactly one flow. After an update, the updater relaunches
/// the new Pubsplash as soon as it believes the old one is gone — and on
/// Windows its `wait_for_exit` reports "already gone" when `OpenProcess` fails,
/// gives up after a 90 s timeout, and on the installed path hands off to NSIS,
/// whose "run now" is not under our control at all. Without this window a
/// well-behaved update would sometimes end in an "already running" box and no
/// app. Do not remove it as dead weight; nothing else needs it, and the cost is
/// a short pause in front of a message box nobody wanted anyway.
const GRACE: Duration = Duration::from_secs(2);

const POLL: Duration = Duration::from_millis(100);

pub enum Acquired {
    Held(InstanceGuard),
    AlreadyRunning,
}

/// Claims the right to be the one running Pubsplash for this user.
pub fn acquire() -> Acquired {
    acquire_named(imp::CLAIM_NAME, GRACE)
}

/// A kernel named mutex. Handed out whether or not the object already existed,
/// so the answer is in the last error.
#[cfg(windows)]
mod imp {
    use super::{Acquired, POLL};
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONINFORMATION, MB_OK, MB_SYSTEMMODAL, MessageBoxW,
    };
    use windows_core::PCWSTR;

    /// `Local\` is the session namespace, so two people signed in to one machine
    /// (fast user switching, or RDP) each get their own — which matches the fact
    /// that config, logs and credentials are all per-user. The name carries no
    /// version and no path on purpose: that is what makes every install of
    /// Pubsplash contend for the same object.
    pub const CLAIM_NAME: &str = r"Local\Pubsplash-SingleInstance";

    /// A held claim on the single-instance mutex, released when it is dropped.
    pub struct InstanceGuard {
        handle: HANDLE,
    }

    impl Drop for InstanceGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }

    /// The body of `acquire`, with the name and grace window as arguments so
    /// the tests can contend for an object of their own.
    pub fn acquire_named(name: &str, grace: Duration) -> Acquired {
        let deadline = Instant::now() + grace;
        let wide_name = wide(name);
        loop {
            // A valid handle comes back whether or not the object already
            // existed, so the answer is in the last error and the handle has to
            // be closed again before the next attempt.
            let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide_name.as_ptr())) };
            let handle = match handle {
                Ok(handle) => handle,
                // Nothing a user can act on, and refusing to start over it would
                // be worse than the duplicate it was meant to prevent.
                Err(_) => return Acquired::AlreadyRunning,
            };
            if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
                return Acquired::Held(InstanceGuard { handle });
            }
            unsafe {
                let _ = CloseHandle(handle);
            }
            if Instant::now() >= deadline {
                return Acquired::AlreadyRunning;
            }
            std::thread::sleep(POLL);
        }
    }

    /// Tells the user why this copy is not starting, and stops.
    ///
    /// A raw `MessageBoxW` because there is no wx app yet — this runs before
    /// anything in `main` — and system-modal because there is no other surface
    /// for it to sit on. It also goes to stderr, which costs nothing and is the
    /// only way to read it from a console; release builds are
    /// `windows_subsystem = "windows"` and have no console for it to reach, so
    /// no user sees it twice.
    pub fn reject_and_exit() -> ! {
        let text = wide(super::MESSAGE);
        let caption = wide("Pubsplash");
        eprintln!("pubsplash: {}", super::MESSAGE);
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_OK | MB_ICONINFORMATION | MB_SYSTEMMODAL,
            );
        }
        std::process::exit(0);
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

/// An advisory lock on a file in the data directory.
///
/// `File::try_lock` is the whole mechanism — std's own `flock` wrapper, which
/// has been stable since 1.89. The lock is owned by the open file
/// *description*, so it is released by the kernel when the process ends
/// for any reason — a crash, a `kill -9`, a Force Quit — which is the property
/// a hand-rolled pid file does not have. The file's contents are never read;
/// only whether the lock can be taken matters, so a file left behind by a
/// previous run is not stale state and is not cleaned up.
#[cfg(target_os = "macos")]
mod imp {
    use super::{Acquired, POLL};
    use std::fs::{File, OpenOptions, TryLockError};
    use std::time::{Duration, Instant};

    /// A file name rather than a kernel object name, but the same rule applies:
    /// no version and no install path, so every copy of Pubsplash this user runs
    /// contends for the same lock. It sits in the data directory, which is
    /// already per-user, so two accounts on one Mac never collide.
    pub const CLAIM_NAME: &str = "single-instance.lock";

    /// A held claim on the lock file. Dropping this closes the descriptor,
    /// which is what releases the lock.
    pub struct InstanceGuard {
        _file: File,
    }

    pub fn acquire_named(name: &str, grace: Duration) -> Acquired {
        let deadline = Instant::now() + grace;
        let path = crate::data_dir::root().join(name);
        // The data directory may not exist yet on a first run; this is above
        // logging, so there is nothing to report a failure to.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        loop {
            match OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path)
            {
                // `try_lock` is std's own `flock` wrapper here. It distinguishes
                // the two failures that matter: `WouldBlock` is another copy
                // holding the lock, which is what the grace window below is for,
                // and anything else is a filesystem problem there is no point
                // retrying.
                Ok(file) => match file.try_lock() {
                    Ok(()) => return Acquired::Held(InstanceGuard { _file: file }),
                    Err(TryLockError::WouldBlock) => {}
                    Err(TryLockError::Error(_)) => return Acquired::AlreadyRunning,
                },
                // Nothing a user can act on, and refusing to start over it would
                // be worse than the duplicate it was meant to prevent.
                Err(_) => return Acquired::AlreadyRunning,
            }
            if Instant::now() >= deadline {
                return Acquired::AlreadyRunning;
            }
            std::thread::sleep(POLL);
        }
    }

    /// Tells the user why this copy is not starting, and stops.
    ///
    /// `osascript` rather than an AppKit alert because there is no `NSApp` yet
    /// — this runs before `wxdragon::main` — and standing one up only to show
    /// one dialog is more machinery than the message is worth. `display alert`
    /// is spoken by VoiceOver like any other alert. If it cannot run, the
    /// stderr line is still there.
    pub fn reject_and_exit() -> ! {
        eprintln!("pubsplash: {}", super::MESSAGE);
        let script = format!(
            r#"display alert "Pubsplash" message "{}" as informational"#,
            super::MESSAGE
        );
        let _ = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .status();
        std::process::exit(0);
    }
}

/// What [`reject_and_exit`] says, in both implementations.
const MESSAGE: &str = "Pubsplash is already running.";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Tests run in parallel threads of one process, and the claim namespace is
    /// shared by all of them, so every test needs a name no other test can
    /// collide with.
    fn unique_name() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "Pubsplash-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        // A kernel object name on Windows, a file name in the data directory on
        // macOS. Neither may be the real claim, or a test would turn the
        // developer's running copy away.
        if cfg!(windows) {
            format!(r"Local\{unique}")
        } else {
            format!("{unique}.lock")
        }
    }

    fn is_held(acquired: &Acquired) -> bool {
        matches!(acquired, Acquired::Held(_))
    }

    #[test]
    fn second_acquire_is_rejected() {
        let name = unique_name();
        let first = acquire_named(&name, Duration::ZERO);
        assert!(is_held(&first), "the first instance should be let in");
        let second = acquire_named(&name, Duration::ZERO);
        assert!(
            !is_held(&second),
            "the second instance should be turned away"
        );
    }

    /// The property the post-update relaunch depends on: the claim is gone the
    /// moment the holder is.
    #[test]
    fn releasing_lets_the_next_one_in() {
        let name = unique_name();
        let first = acquire_named(&name, Duration::ZERO);
        assert!(is_held(&first));
        drop(first);
        let second = acquire_named(&name, Duration::ZERO);
        assert!(is_held(&second), "the claim should have been released");
    }

    #[test]
    fn grace_window_is_waited_out() {
        let name = unique_name();
        let holder = acquire_named(&name, Duration::ZERO);
        assert!(is_held(&holder));
        // The waiter runs on the other thread rather than the holder, because
        // the Windows guard wraps a `HANDLE`, which is not `Send`, and is
        // deliberately not made so for the sake of a test — the app only ever
        // holds it on the main thread.
        let waiting = std::thread::spawn({
            let name = name.clone();
            move || is_held(&acquire_named(&name, Duration::from_secs(2)))
        });
        std::thread::sleep(Duration::from_millis(200));
        drop(holder);
        assert!(
            waiting.join().expect("the waiting thread"),
            "the grace window should have covered the wait"
        );
    }

    /// Two different names are two different claims — this is what keeps the
    /// guard from reaching the other binaries or another user's session.
    ///
    /// On macOS it is also the test that the lock is on the descriptor rather
    /// than on the path: both files exist at once and both are held.
    #[test]
    fn different_names_do_not_contend() {
        let first = acquire_named(&unique_name(), Duration::ZERO);
        let second = acquire_named(&unique_name(), Duration::ZERO);
        assert!(is_held(&first) && is_held(&second));
    }
}
