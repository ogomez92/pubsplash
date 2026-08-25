//! pubsplash-update: applies an update once Pubsplash itself has exited.
//!
//! **Windows only.** This helper exists for one reason: Windows will not let a
//! running executable be replaced, so the last step of an update cannot be done
//! by the process being updated. macOS has no such rule — a running `.app`
//! bundle can be replaced underneath itself — so there the apply step happens
//! in-process and none of this is needed.
//!
//! Everything *before* that last step is shared and stays in `src/update/`: the
//! `latest.json` manifest, the version comparison, the download, and the size
//! and SHA-256 checks. Only the apply differs, and only it lives here.
//!
//! Cargo cannot make a `[[bin]]` target conditional, so the binary is still
//! built everywhere and this file is the seam. The real implementation is
//! [`updater_win`], kept in a file of its own rather than inline so that the
//! Windows code needs no per-item `#[cfg]` and reads exactly as it did before
//! there was a second platform. (`autobins = false` in `Cargo.toml` is what
//! stops that file being auto-discovered as a sixth binary.)
//!
//! On any other platform `main` explains itself and exits non-zero, which is a
//! better answer than a helper that appears to have succeeded.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
#[path = "updater_win.rs"]
mod updater_win;

#[cfg(windows)]
fn main() {
    updater_win::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "pubsplash-update applies the last step of a Windows update from a separate \
         process, because Windows cannot replace a running executable. This platform \
         can, so Pubsplash updates itself and never runs this helper."
    );
    std::process::exit(2);
}
