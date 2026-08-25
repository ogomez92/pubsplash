//! The macOS counterpart of [`super::module_pin`], and it does nothing.
//!
//! On Windows that module takes one permanent extra `LoadLibraryExW` reference
//! per distinct plugin binary, and one permanent `IPluginFactory` reference,
//! because `Vst2Plugin::drop`'s `FreeLibrary` and `vst3-host`'s own teardown
//! would otherwise balance to zero and **unmap the image** — which a plugin with
//! a worker thread, a timer or a live COM stub does not survive. The extra
//! reference puts a floor under the count that no `FreeLibrary` can reach.
//!
//! There is no such hazard here. `vst3-host` does not `dlclose` the bundle it
//! opened, and `dlclose` on macOS is advisory in any case — the dynamic linker
//! is free to keep an image mapped and routinely does, particularly for anything
//! that has registered an Objective-C class or a thread-local destructor, which
//! every plugin bundle has. Nothing can pull the image out from under a
//! straggling thread, so nothing needs pinning to stop it.
//!
//! Kept as a module with the same two functions so `host3` and the loaders call
//! it unconditionally, and so the reasoning above sits next to the reasoning it
//! contrasts with rather than in a commit message.

use std::path::{Path, PathBuf};

/// Answers the caller's real question — where the binary is — and holds nothing
/// down. The Windows version returns the same path after taking its reference,
/// so callers read alike.
pub fn pin(path: &str) -> PathBuf {
    Path::new(path).to_path_buf()
}

/// A shell module's factory needs no extra reference here; see the header.
pub fn pin_factory(_path: &str) {}
