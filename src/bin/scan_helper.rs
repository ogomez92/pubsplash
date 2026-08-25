//! pubsplash-scan: loads a single VST plugin and prints its info as JSON.
//!
//! Runs as a separate process so a plugin that crashes or hangs while loading
//! cannot take Pubsplash down with it. Activation dialogs a plugin shows
//! during load appear normally (same desktop); Pubsplash waits.
//!
//! Usage: `pubsplash-scan --vst2 <dll>` or `pubsplash-scan --vst3 <binary>`.
//! Success: JSON on stdout, exit 0. Failure: reason on stderr, exit nonzero.
//!
//! `--vst2` exists on Windows only, which is the only platform Pubsplash hosts
//! that format on. It is still accepted here rather than removed, so a stale
//! plugin-cache entry or a hand-run command gets a message rather than a usage
//! error.

#[allow(dead_code)]
#[path = "../vst/types.rs"]
mod types;
#[cfg(windows)]
#[path = "../vst/vst2.rs"]
mod vst2;
#[path = "../vst/vst3.rs"]
mod vst3;

use std::io::Write;
use std::path::Path;

/// Flushes stdio and kills this process without running the loaded modules'
/// teardown.
///
/// A normal exit notifies every loaded library — `DLL_PROCESS_DETACH` on
/// Windows, static destructors and `atexit` handlers on macOS — and plugins
/// routinely crash there (static destructors, orphaned worker threads), which
/// used to turn a fully successful probe into a fault exit code. `_exit` is the
/// POSIX name for the same idea `TerminateProcess` serves.
#[cfg(windows)]
fn exit_hard(code: u32) -> ! {
    use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    unsafe {
        let _ = TerminateProcess(GetCurrentProcess(), code);
    }
    unreachable!("TerminateProcess on the current process does not return");
}

#[cfg(target_os = "macos")]
fn exit_hard(code: u32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: `_exit` never returns and runs no user code on the way out, which
    // is the whole point.
    unsafe { libc::_exit(code as i32) }
}

/// Keeps the OS from putting a dialog in front of the user when a plugin faults
/// this process; the parent reports the failure instead. Also gives plugins the
/// runtime environment they expect during load.
#[cfg(windows)]
fn quieten_the_os() {
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::System::Diagnostics::Debug::{
        SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
    };
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
        // Some plugins use COM while loading and fault on an uninitialized
        // apartment.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// Nothing to do on macOS. There is no COM apartment to enter, and the crash
/// reporter does not interrupt a process that has no bundle and no
/// `NSApplication` — it writes its `.ips` report and lets the parent see the
/// signal exit code, which is exactly the behaviour wanted.
#[cfg(target_os = "macos")]
fn quieten_the_os() {}

fn main() {
    quieten_the_os();

    let args: Vec<String> = std::env::args().collect();
    let (flag, path) = match (args.get(1), args.get(2)) {
        (Some(flag), Some(path)) => (flag.as_str(), Path::new(path)),
        _ => {
            eprintln!("usage: pubsplash-scan --vst2 <dll> | --vst3 <binary>");
            std::process::exit(2);
        }
    };
    let result = match flag {
        #[cfg(windows)]
        "--vst2" => vst2::scan(path),
        #[cfg(not(windows))]
        "--vst2" => Err("VST2 is not supported on this platform".to_string()),
        "--vst3" => vst3::scan(path),
        _ => {
            eprintln!("unknown flag {flag}");
            std::process::exit(2);
        }
    };
    match result {
        Ok(out) => {
            println!("{}", serde_json::to_string(&out).expect("serializable"));
            exit_hard(0);
        }
        Err(reason) => {
            eprintln!("{reason}");
            exit_hard(1);
        }
    }
}
