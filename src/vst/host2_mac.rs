//! VST2 hosting on macOS: **deliberately absent**, and shaped so that saying so
//! costs no `cfg` anywhere else.
//!
//! The decision is that Pubsplash 1.0 on macOS supports VST3 only. VST2 is a
//! format Steinberg discontinued, its licence has not been issued for years, and
//! a macOS VST2 is a Mach-O inside a `.vst` bundle reached through `CFBundle` —
//! so supporting it means writing an entirely new loader (the Windows side loads
//! a DLL and parses PE headers to check the architecture first) for a format no
//! new plugin ships in. Audio Units are what Mac users actually have installed,
//! and that is where the effort belongs when there is effort to spend. See the
//! port assessment.
//!
//! The interesting part is how that is expressed. `PluginInstance` is an enum
//! with a `Vst2` arm and about thirty match arms reaching into it, so making the
//! *variant* conditional would put a `#[cfg]` on every one of them and on every
//! caller that constructs one. Instead the variant stays and its payload becomes
//! **uninhabited**: [`Vst2Plugin`] carries a private field of an empty enum, so
//! the compiler knows no value of it can exist, while `plugin.info` and
//! `plugin.get_parameter(..)` still typecheck exactly as they do on Windows.
//!
//! That gives three things at once. Every match arm compiles unchanged. Nothing
//! can construct one — [`Vst2Plugin::load`] is the only route and it returns an
//! error — so the `unreachable!` bodies below are statically unreachable rather
//! than a promise. And `PluginInstance::Vst2` becomes an uninhabited variant,
//! which costs nothing at runtime.
//!
//! Discovery never offers a VST2 candidate here anyway (`discover` looks for
//! `.dll`), so `load` is not reached even by a stale plugin cache entry pointing
//! at a Windows plugin — but it answers with a real message if it ever is.

use std::ffi::c_void;

use crate::config::{FxSlotConfig, ParamValue, PluginRef};
use crate::vst::PluginInfo;
use crate::vst::instance::Processed;

/// Uninhabited, which is what makes [`Vst2Plugin`] uninhabited.
enum Never {}

/// What a VST2 plugin would be if this platform hosted them. No value of this
/// type can be constructed; see the module header.
///
/// Every field mirrors the Windows struct so the callers reading them typecheck.
/// Some are read there and not here, which is the point rather than an oversight.
#[allow(dead_code)]
pub struct Vst2Plugin {
    pub info: PluginRef,
    pub num_params: i32,
    pub num_inputs: i32,
    pub num_outputs: i32,
    pub has_editor: bool,
    pub uses_chunks: bool,
    /// The field that makes the struct uninhabited. Never named anywhere else.
    _never: Never,
}

/// Every method below is reachable only through a value that cannot exist.
macro_rules! unreachable_without_a_plugin {
    () => {
        unreachable!("Vst2Plugin is uninhabited on this platform; see host2_mac.rs")
    };
}

impl Vst2Plugin {
    /// The one route to a `Vst2Plugin`, and it always refuses.
    pub fn load(info: &PluginInfo, _slot: &FxSlotConfig) -> Result<Vst2Plugin, String> {
        Err(format!(
            "{} is a VST2 plugin, and Pubsplash on macOS hosts VST3 only",
            info.path
        ))
    }

    pub fn get_parameter(&self, _index: i32) -> f32 {
        unreachable_without_a_plugin!()
    }

    pub fn set_parameter(&self, _index: i32, _value: f32) {
        unreachable_without_a_plugin!()
    }

    pub fn param_name(&self, _index: i32) -> String {
        unreachable_without_a_plugin!()
    }

    pub fn param_display(&self, _index: i32) -> String {
        unreachable_without_a_plugin!()
    }

    pub fn param_label(&self, _index: i32) -> String {
        unreachable_without_a_plugin!()
    }

    pub fn string_to_parameter(&self, _index: i32, _text: &str) -> bool {
        unreachable_without_a_plugin!()
    }

    pub fn can_be_automated(&self, _index: i32) -> bool {
        unreachable_without_a_plugin!()
    }

    pub fn snapshot(&self) -> (Option<String>, Vec<ParamValue>) {
        unreachable_without_a_plugin!()
    }

    pub fn editor_rect(&self) -> Option<(i32, i32)> {
        unreachable_without_a_plugin!()
    }

    pub fn editor_open(&self, _handle: *mut c_void) {
        unreachable_without_a_plugin!()
    }

    pub fn editor_close(&self) {
        unreachable_without_a_plugin!()
    }

    pub fn editor_idle(&self) {
        unreachable_without_a_plugin!()
    }

    pub fn effect_id(&self) -> u64 {
        unreachable_without_a_plugin!()
    }

    /// # Safety
    /// Unreachable, so there is nothing to uphold. The signature matches the
    /// Windows one so `PluginInstance::process` needs no `cfg`.
    pub unsafe fn process_replacing(
        &self,
        _inputs: *const *mut f32,
        _outputs: *const *mut f32,
        _frames: i32,
    ) -> Processed {
        unreachable_without_a_plugin!()
    }
}

/// The engine calls this once per block whatever the platform. VST2's host
/// transport is a global sample position kept for `audioMasterGetTime`, and
/// with no VST2 instance to ask for it there is nothing to advance.
pub fn advance_transport(_frames: u64) {}

/// Drained from the UI pump every 100 ms. VST2 editors ask to be resized
/// through `audioMasterSizeWindow`; no VST2 editor exists here, so the queue is
/// always empty.
pub fn take_size_requests() -> Vec<(u64, i32, i32)> {
    Vec::new()
}
