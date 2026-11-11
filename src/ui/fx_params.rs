//! An OSARA-style parameter dialog: a screen-reader-friendly way to adjust
//! any plugin's automation parameters without its native UI. Modelled on the
//! REAPER OSARA extension's FX parameter dialog.
//!
//! The dialog's logic lives behind the [`ParamSource`] trait so the stepping,
//! filtering, and value-formatting rules can be unit-tested against a fake.

use crate::t;
use super::fx::{self, ChainTarget};
use super::slider_keys::key_step;
use super::slider_uia::{self, SliderAnnouncer};
use super::{
    App, WXK_DOWN, WXK_END, WXK_ESCAPE, WXK_HOME, WXK_LEFT, WXK_PAGEDOWN, WXK_PAGEUP, WXK_RIGHT,
    WXK_TAB, WXK_UP,
};
use crate::vst::PluginInstance;
use std::rc::Rc;
use std::sync::Arc;
use wxdragon::prelude::*;

/// Updates the slider position and value text for a parameter, and announces
/// the new value to screen readers.
///
/// The formatted value (the plugin's own text, e.g. "-3.0 dB", falling back to
/// a percentage only when the plugin reports none) is pushed to the slider's
/// UIA provider ([`SliderAnnouncer`]), which exposes it as the slider's value
/// and raises a value-change event so a screen reader speaks it on each keyboard
/// step. The read-only value text mirrors it as a tab-to-read fallback.
///
/// The slider is re-ranged every time, not only when the parameter changes:
/// the range *is* the parameter's shape (see [`slider_range`]), and setting it
/// to what it already is costs nothing.
fn announce_value(
    announcer: &SliderAnnouncer,
    slider: &Slider,
    value_text: &TextCtrl,
    src: &dyn ParamSource,
    index: i32,
) {
    let text = sync_value(announcer, slider, value_text, src, index);
    announcer.update(&src.name(index), &text);
}

/// Brings the slider, its accessible value and the value text into line with
/// the parameter, raising no event, and returns the value's text.
///
/// The silent half of [`announce_value`], for a change made from somewhere
/// other than the slider: a value-change event is only spoken for the control
/// that has focus, so raising one on the slider from the value-entry field
/// says nothing and the caller has to speak by another route.
fn sync_value(
    announcer: &SliderAnnouncer,
    slider: &Slider,
    value_text: &TextCtrl,
    src: &dyn ParamSource,
    index: i32,
) -> String {
    let range = slider_range(src.steps(index));
    let text = formatted_value(src, index);
    slider.set_range(0, range);
    slider.set_value(slider_position(src.get(index), range));
    value_text.set_value(&text);
    announcer.set_text(&src.name(index), &text);
    text
}

/// Keyboard step sizes as fractions of the normalized 0..1 range (OSARA uses
/// 1/1000 for arrows, 1/50 for page keys).
const SMALL_STEP: f32 = 0.001;
const LARGE_STEP: f32 = 0.02;
/// The value slider's top position for a continuous parameter, and the most
/// steps a parameter may have and still get a position of its own for each.
const SLIDER_RANGE: i32 = 1000;
/// How many extra steps to try before giving up on finding a value whose
/// formatted text differs (so discrete parameters move one detent per press).
const MAX_SNAP_TRIES: u32 = 10;

/// What the dialog needs from a plugin. Implemented for a real plugin and for
/// a test fake.
pub trait ParamSource {
    fn count(&self) -> i32;
    fn name(&self, index: i32) -> String;
    fn get(&self, index: i32) -> f32;
    fn set(&self, index: i32, value: f32);
    fn display(&self, index: i32) -> String;
    fn label(&self, index: i32) -> String;
    fn automatable(&self, index: i32) -> bool;
    /// Parses `text` in the parameter's own display units (e.g. "-6 dB") and
    /// applies it, returning whether the plugin supported the conversion. VST2
    /// exposes this as an optional opcode; VST3 has no way to be asked, which
    /// is what [`find_step`] makes up for where it can.
    fn set_from_string(&self, index: i32, text: &str) -> bool;
    /// How many steps the parameter has above its lowest position, or 0 when
    /// it is continuous (or the format cannot say). A switch is 1; a list of
    /// twenty names is 19.
    fn steps(&self, index: i32) -> u32;
    /// The text the plugin would show at `value`, without moving the parameter
    /// there. `None` when the format can only describe the current value.
    fn display_at(&self, index: i32, value: f32) -> Option<String>;
}

impl ParamSource for Arc<PluginInstance> {
    fn count(&self) -> i32 {
        self.num_params()
    }
    fn name(&self, index: i32) -> String {
        self.param_name(index)
    }
    fn get(&self, index: i32) -> f32 {
        self.get_parameter(index)
    }
    fn set(&self, index: i32, value: f32) {
        self.set_parameter(index, value);
    }
    fn display(&self, index: i32) -> String {
        self.param_display(index)
    }
    fn label(&self, index: i32) -> String {
        self.param_label(index)
    }
    fn automatable(&self, index: i32) -> bool {
        self.can_be_automated(index)
    }
    fn set_from_string(&self, index: i32, text: &str) -> bool {
        self.string_to_parameter(index, text)
    }
    fn steps(&self, index: i32) -> u32 {
        self.param_steps(index)
    }
    fn display_at(&self, index: i32, value: f32) -> Option<String> {
        self.param_display_at(index, value)
    }
}

/// The value slider's top position for a parameter with `steps` steps.
///
/// A stepped parameter gets exactly one position per value, so the slider *is*
/// the parameter: a list of 53 voices is a slider of 53 positions, each one a
/// name, and one arrow press is one voice. It used to be 1000 positions
/// whatever the parameter was, which left nineteen dead positions between each
/// pair of voices for a mouse to land on and made every arrow press a search
/// for the next one.
///
/// A continuous parameter, or one with more steps than a slider can usefully
/// have, keeps the 1000.
pub fn slider_range(steps: u32) -> i32 {
    if is_stepped(steps) {
        steps as i32
    } else {
        SLIDER_RANGE
    }
}

/// Whether a parameter is moved one of its own steps at a time, rather than by
/// a fraction of its range.
pub fn is_stepped(steps: u32) -> bool {
    (1..=SLIDER_RANGE as u32).contains(&steps)
}

/// Where the slider sits for a normalized value.
pub fn slider_position(value: f32, range: i32) -> i32 {
    (value.clamp(0.0, 1.0) * range as f32).round() as i32
}

/// The normalized value of a slider position — `k / n`, which is what VST3
/// defines step `k` of `n` to be.
pub fn slider_value(position: i32, range: i32) -> f32 {
    position.clamp(0, range) as f32 / range.max(1) as f32
}

/// How far Page Up and Page Down move a stepped parameter: a tenth of the way,
/// and never nothing.
fn page_step(range: i32) -> i32 {
    (range / 10).max(1)
}

/// Applies one movement key to a parameter and returns its new normalized
/// value, or `None` when `code` is not a movement key — the caller must then
/// let the event through.
///
/// Every movement key is owned here, Left and Right included. Leaving any of
/// them to the native trackbar moves the thumb without telling the plugin, so
/// the slider shows one value while the plugin holds another and nothing is
/// announced at all.
///
/// The directions and the Home/End mapping are [`key_step`]'s, like every
/// other slider in the app. A continuous parameter keeps OSARA's fractional
/// steps and its search for the next value that reads differently
/// ([`step_param`]); a stepped one has no need of a search.
pub fn key_param(src: &dyn ParamSource, index: i32, code: i32) -> Option<f32> {
    let steps = src.steps(index);
    if is_stepped(steps) {
        let range = slider_range(steps);
        let position = slider_position(src.get(index), range);
        let next = key_step(code, position, 0, range, page_step(range))?;
        return Some(set_param(src, index, slider_value(next, range)));
    }
    Some(match code {
        WXK_UP | WXK_RIGHT => step_param(src, index, SMALL_STEP),
        WXK_DOWN | WXK_LEFT => step_param(src, index, -SMALL_STEP),
        WXK_PAGEUP => step_param(src, index, LARGE_STEP),
        WXK_PAGEDOWN => step_param(src, index, -LARGE_STEP),
        WXK_HOME => set_param(src, index, 1.0),
        WXK_END => set_param(src, index, 0.0),
        _ => return None,
    })
}

/// What happened to a typed value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TypedValue {
    /// The text was understood; the new normalized value is attached.
    Applied(f32),
    /// The field was blank, so there was nothing to apply.
    Blank,
    /// The parameter's values could all be read, and none of them is this.
    NoSuchValue,
    /// The plugin cannot convert display text back to a value.
    Unsupported,
}

/// What became of looking a typed name up among a stepped parameter's values.
#[derive(Debug, Clone, Copy, PartialEq)]
enum StepSearch {
    Found(f32),
    /// Every value was read and none matched.
    NotThere,
    /// Not a stepped parameter, or the plugin would not name its values.
    CannotSearch,
}

/// Finds the step of a stepped parameter whose text is `typed`.
///
/// This is what makes typing "Chipmunk" work for a format that cannot be asked
/// to parse text: a stepped parameter has few enough values to ask for each
/// one's text and compare. A whole match wins; failing that, a prefix that
/// only one value starts with, so "chip" is enough when nothing else begins
/// that way. Case is ignored, and the unit may be typed or left off.
fn find_step(src: &dyn ParamSource, index: i32, typed: &str) -> StepSearch {
    let steps = src.steps(index);
    if !is_stepped(steps) {
        return StepSearch::CannotSearch;
    }
    let range = slider_range(steps);
    let unit = src.label(index).trim().to_lowercase();
    let mut wanted = typed.trim().to_lowercase();
    if !unit.is_empty()
        && let Some(bare) = wanted.strip_suffix(&unit)
    {
        wanted = bare.trim_end().to_string();
    }
    let mut texts = Vec::with_capacity(range as usize + 1);
    for position in 0..=range {
        match src.display_at(index, slider_value(position, range)) {
            Some(text) => texts.push(text.trim().to_lowercase()),
            None => return StepSearch::CannotSearch,
        }
    }
    let position = texts.iter().position(|text| *text == wanted).or_else(|| {
        let mut starting = texts
            .iter()
            .enumerate()
            .filter(|(_, text)| text.starts_with(&wanted));
        match (starting.next(), starting.next()) {
            (Some((position, _)), None) => Some(position),
            _ => None,
        }
    });
    match position {
        Some(position) => StepSearch::Found(slider_value(position as i32, range)),
        None => StepSearch::NotThere,
    }
}

/// Applies `text` (in the parameter's display units) via the plugin's optional
/// string-to-parameter conversion, or failing that by finding the step of a
/// stepped parameter that reads that way ([`find_step`]).
///
/// The outcomes are deliberately distinct: silently reverting the field is
/// indistinguishable from success to a screen-reader user, and a field that
/// swallows everything typed into it is a dead control.
pub fn apply_typed_value(src: &dyn ParamSource, index: i32, text: &str) -> TypedValue {
    let text = text.trim();
    if text.is_empty() {
        return TypedValue::Blank;
    }
    if src.set_from_string(index, text) {
        return TypedValue::Applied(src.get(index));
    }
    match find_step(src, index, text) {
        StepSearch::Found(value) => TypedValue::Applied(set_param(src, index, value)),
        StepSearch::NotThere => TypedValue::NoSuchValue,
        StepSearch::CannotSearch => TypedValue::Unsupported,
    }
}

/// The human-readable value: the plugin's formatted text plus its unit, or a
/// percentage fallback when the plugin formats nothing.
pub fn formatted_value(src: &dyn ParamSource, index: i32) -> String {
    let display = src.display(index);
    if display.is_empty() {
        return format!("{}%", (src.get(index) * 100.0).round() as i32);
    }
    let label = src.label(index);
    if label.is_empty() {
        display
    } else {
        t!("{display} {label}", display = display, label = label)
    }
}

/// Whether a parameter name is meaningful (not blank or a bare number). OSARA
/// hides "unnamed" parameters by default.
pub fn is_named(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty() && !trimmed.chars().all(|c| c.is_ascii_digit())
}

/// One parameter's unchanging metadata.
pub struct ParamInfo {
    pub index: i32,
    pub name: String,
    /// Lowercased once, because the filter box compares against it on every
    /// keystroke.
    pub search_name: String,
    pub automatable: bool,
}

/// Reads every parameter's name and automatability.
///
/// Both are dispatcher round-trips into the plugin, and the filter box
/// re-filters on every keystroke — so re-reading them per keystroke cost a
/// 300-parameter plugin around 900 round-trips per character typed, in the one
/// dialog built specifically for screen-reader users. The dialog reads this
/// once when it opens and filters over the result.
///
/// The trade-off is that a plugin renaming its parameters while the dialog is
/// open shows stale names until it is reopened. That is rare, and the host
/// already ignores `AUDIO_MASTER_UPDATE_DISPLAY` (see `vst::host2`).
pub fn param_infos(src: &dyn ParamSource) -> Vec<ParamInfo> {
    (0..src.count())
        .map(|index| {
            let name = src.name(index);
            ParamInfo {
                index,
                search_name: name.to_lowercase(),
                name,
                automatable: src.automatable(index),
            }
        })
        .collect()
}

/// The parameter indices the dialog should show: automatable, name-matched,
/// and (unless `include_unnamed`) meaningfully named.
pub fn filter_infos(infos: &[ParamInfo], filter: &str, include_unnamed: bool) -> Vec<i32> {
    let filter = filter.trim().to_lowercase();
    infos
        .iter()
        .filter(|info| info.automatable)
        .filter(|info| {
            if !include_unnamed && !is_named(&info.name) {
                return false;
            }
            filter.is_empty() || info.search_name.contains(&filter)
        })
        .map(|info| info.index)
        .collect()
}

/// [`filter_infos`] straight off a plugin, reading its metadata as it goes.
/// The dialog caches via [`param_infos`] instead; this is for tests.
#[cfg(test)]
pub fn filter_params(src: &dyn ParamSource, filter: &str, include_unnamed: bool) -> Vec<i32> {
    filter_infos(&param_infos(src), filter, include_unnamed)
}

/// Applies one keyboard step and returns the new normalized value. Moves to
/// the next value whose formatted text differs from the current one: it tries
/// `delta` first (staying precise for finely-quantized parameters) and, if
/// the text is unchanged, grows the step geometrically (up to
/// [`MAX_SNAP_TRIES`] times) so a coarse discrete parameter still advances one
/// detent per keypress.
pub fn step_param(src: &dyn ParamSource, index: i32, delta: f32) -> f32 {
    let start = src.get(index);
    let start_text = formatted_value(src, index);
    let mut step = delta;
    let mut value = (start + step).clamp(0.0, 1.0);
    src.set(index, value);
    for _ in 0..MAX_SNAP_TRIES {
        if value <= 0.0 || value >= 1.0 || formatted_value(src, index) != start_text {
            break;
        }
        step *= 2.0;
        value = (start + step).clamp(0.0, 1.0);
        src.set(index, value);
    }
    value
}

/// Sets an absolute normalized value (Home/End) and returns it.
pub fn set_param(src: &dyn ParamSource, index: i32, value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    src.set(index, value);
    value
}

/// Opens the parameter dialog for a plugin instance. On close, the slot's
/// state is snapshotted back into config.
pub fn edit_parameters(
    app: &Rc<App>,
    target: ChainTarget,
    slot: usize,
    plugin: Arc<PluginInstance>,
) {
    let Some(frame) = app.widgets(|w| w.frame) else {
        return;
    };
    // Re-read the parameter list before showing it. A plugin may change its
    // parameter set at runtime — loading a preset in its own interface is
    // enough — and VST3 gives us no notification when it does. A stale
    // index-to-id map would have the dialog announce one parameter's name
    // while an arrow key edits a different parameter, which a screen-reader
    // user has no way to notice.
    plugin.refresh_params();
    let plugin_name = plugin.info().name.clone();
    let src: Rc<dyn ParamSource> = Rc::new(plugin);
    if src.count() == 0 {
        super::show_info(
            &frame,
            &t!("Edit parameters"),
            &t!("This plugin exposes no automatable parameters."),
        );
        return;
    }

    // Named for the plugin: this dialog is reached from a list of several, and
    // its title is the first thing read when it opens.
    let title = if plugin_name.trim().is_empty() {
        t!("Plugin parameters")
    } else {
        t!("{name} parameters", name = plugin_name)
    };
    let dialog = Dialog::builder(&frame, &title)
        .with_style(DialogStyle::DefaultDialogStyle)
        .with_size(460, 320)
        .build();
    let panel = Panel::builder(&dialog).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let filter_label = StaticText::builder(&panel).with_label(&t!("Filter")).build();
    let filter_box = TextCtrl::builder(&panel).build();
    super::set_accessible_name(&filter_box, &t!("Filter parameters"));
    super::help::tag(
        &filter_box,
        "dialog.fxParams.filter",
        "Filter parameters by name box",
    );

    let param_label = StaticText::builder(&panel).with_label(&t!("Parameter")).build();
    let param_choice = Choice::builder(&panel).build();
    super::set_accessible_name(&param_choice, &t!("Parameter"));
    super::help::tag(
        &param_choice,
        "dialog.fxParams.parameter",
        "Plugin parameter chooser",
    );

    let value_label = StaticText::builder(&panel).with_label(&t!("Value")).build();
    let value_slider = Slider::builder(&panel)
        .with_value(0)
        .with_min_value(0)
        .with_max_value(1000)
        .build();
    super::set_accessible_name(&value_slider, &t!("Value"));
    super::help::tag(
        &value_slider,
        "dialog.fxParams.value",
        "Selected parameter value slider",
    );
    // Give the slider a UIA provider so keyboard steps announce the plugin's
    // formatted value (see `slider_uia`). Kept alive until the dialog closes.
    let announcer = Rc::new(slider_uia::install(&value_slider));

    let value_text = TextCtrl::builder(&panel)
        .with_style(TextCtrlStyle::ProcessEnter)
        .build();
    super::set_accessible_name(&value_text, &t!("Value entry"));
    super::help::tag(
        &value_text,
        "dialog.fxParams.valueText",
        "Type-in value entry for the selected parameter",
    );

    let unnamed_check = CheckBox::builder(&panel)
        .with_label(&t!("Show unnamed parameters"))
        .build();
    super::set_accessible_name(&unnamed_check, &t!("Show unnamed parameters"));
    super::help::tag(
        &unnamed_check,
        "dialog.fxParams.showUnnamed",
        "Show unnamed parameters checkbox",
    );

    // Dismiss-only: `dismiss_button` gives it the `ID_CANCEL` wx maps Escape to and
    // makes it the default item Enter fires. The per-control handlers below also
    // close on Escape, but only on the controls they are attached to; this covers
    // the rest of the dialog. Enter in `value_text` is unaffected — that control
    // has `ProcessEnter` and keeps the key to commit the typed value.
    let close = super::dismiss_button(&panel, &t!("Close"));

    sizer.add(&filter_label, 0, SizerFlag::All, 4);
    sizer.add(&filter_box, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&param_label, 0, SizerFlag::All, 4);
    sizer.add(&param_choice, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&value_label, 0, SizerFlag::All, 4);
    sizer.add(&value_slider, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&value_text, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&unnamed_check, 0, SizerFlag::All, 8);
    sizer.add(&close, 0, SizerFlag::AlignRight | SizerFlag::All, 4);
    panel.set_sizer(sizer, true);
    let dialog_sizer = BoxSizer::builder(Orientation::Vertical).build();
    dialog_sizer.add(&panel, 1, SizerFlag::Expand, 0);
    dialog.set_sizer(dialog_sizer, true);

    // Visible parameter indices, refreshed by the filter and unnamed toggle.
    let visible: Rc<std::cell::RefCell<Vec<i32>>> = Rc::new(std::cell::RefCell::new(Vec::new()));
    // Parameter names and automatability, read once — see `param_infos`.
    let infos = Rc::new(param_infos(src.as_ref()));

    // Updates the slider position, value text, and accessible value for the
    // currently selected parameter.
    let show_value = {
        let src = src.clone();
        let visible = visible.clone();
        let announcer = announcer.clone();
        move || {
            let Some(sel) = param_choice.get_selection() else {
                return;
            };
            let Some(&index) = visible.borrow().get(sel as usize) else {
                return;
            };
            announce_value(&announcer, &value_slider, &value_text, src.as_ref(), index);
        }
    };

    let repopulate = {
        let infos = infos.clone();
        let visible = visible.clone();
        let show_value = show_value.clone();
        move || {
            let filter = filter_box.get_value();
            let indices = filter_infos(&infos, &filter, unnamed_check.get_value());
            *visible.borrow_mut() = indices.clone();
            param_choice.clear();
            for &index in &indices {
                // `param_infos` walks `0..count` in order, so position is index.
                let name = infos
                    .get(index as usize)
                    .map(|info| info.name.as_str())
                    .unwrap_or_default();
                param_choice.append(name);
            }
            if !indices.is_empty() {
                param_choice.set_selection(0);
            }
            show_value();
        }
    };
    repopulate();

    {
        let show_value = show_value.clone();
        param_choice
            .clone()
            .on_selection_changed(move |_| show_value());
    }
    {
        let repopulate = repopulate.clone();
        filter_box.clone().on_text_changed(move |_| repopulate());
    }
    {
        let repopulate = repopulate.clone();
        unnamed_check.clone().on_toggled(move |_| repopulate());
    }

    // Moves to the next/previous visible parameter (Ctrl+Tab), wrapping.
    let cycle_param = {
        let visible = visible.clone();
        let show_value = show_value.clone();
        move |forward: bool| {
            let count = visible.borrow().len();
            if count == 0 {
                return;
            }
            let current = param_choice.get_selection().unwrap_or(0) as usize;
            let next = if forward {
                (current + 1) % count
            } else {
                (current + count - 1) % count
            };
            param_choice.set_selection(next as u32);
            show_value();
        }
    };

    // The slider drives parameter values with OSARA's key mapping.
    {
        let src = src.clone();
        let visible = visible.clone();
        let cycle_param = cycle_param.clone();
        let announcer = announcer.clone();
        value_slider.clone().on_key_down(move |event| {
            let Some((code, ctrl)) = super::key_of(&event) else {
                event.skip(true);
                return;
            };
            // wxDragon re-arms `Skip(true)` before calling every closure, so
            // every key we act on needs an explicit `skip(false)`; otherwise the
            // native trackbar also handles it and drags the thumb away from the
            // value we just announced.
            if code == WXK_ESCAPE {
                event.skip(false);
                dialog.end_modal(ID_OK);
                return;
            }
            if code == WXK_TAB {
                if ctrl {
                    event.skip(false);
                    cycle_param(!event_shift(&event));
                } else {
                    event.skip(true);
                }
                return;
            }
            let Some(sel) = param_choice.get_selection() else {
                event.skip(true);
                return;
            };
            let Some(&index) = visible.borrow().get(sel as usize) else {
                event.skip(true);
                return;
            };
            if key_param(src.as_ref(), index, code).is_none() {
                event.skip(true);
                return;
            }
            event.skip(false);
            // Announced even at the end of the range, where nothing moved, so
            // the key always produces spoken feedback.
            announce_value(&announcer, &value_slider, &value_text, src.as_ref(), index);
        });
    }

    // The mouse, the wheel, and anything else that moves the thumb without a
    // key we own. The thumb is already where the user put it, so the parameter
    // follows the slider here rather than the other way round — and the slider
    // is not repositioned, which would fight a drag in progress.
    {
        let src = src.clone();
        let visible = visible.clone();
        let announcer = announcer.clone();
        value_slider.clone().on_slider(move |_| {
            let Some(sel) = param_choice.get_selection() else {
                return;
            };
            let Some(&index) = visible.borrow().get(sel as usize) else {
                return;
            };
            let range = slider_range(src.steps(index));
            set_param(
                src.as_ref(),
                index,
                slider_value(value_slider.value(), range),
            );
            let text = formatted_value(src.as_ref(), index);
            value_text.set_value(&text);
            announcer.update(&src.name(index), &text);
        });
    }

    // The value-entry field: reads the current value (kept in sync by
    // `announce_value`), and on tab-away or Enter parses what was typed, in the
    // parameter's own display units, and applies it.
    //
    // Every outcome is *said*, and through `help::announce` rather than the
    // slider's own value event: focus is in this field, and a screen reader
    // speaks a value change only for the control that has it. `set_value`
    // alone announces nothing either, so reverting the field in silence would
    // be indistinguishable from the edit having worked.
    let apply_typed = {
        let src = src.clone();
        let visible = visible.clone();
        let announcer = announcer.clone();
        move || {
            let Some(sel) = param_choice.get_selection() else {
                return;
            };
            let Some(&index) = visible.borrow().get(sel as usize) else {
                return;
            };
            let current = formatted_value(src.as_ref(), index);
            let typed = value_text.get_value();
            // Tabbing through the field without touching it is not an edit.
            // Re-applying its own text would be harmless for a parameter whose
            // text parses back exactly, and a silent change of value for one
            // whose text is rounded.
            if typed.trim() == current.trim() {
                return;
            }
            match apply_typed_value(src.as_ref(), index, &typed) {
                TypedValue::Applied(_) => {
                    let text =
                        sync_value(&announcer, &value_slider, &value_text, src.as_ref(), index);
                    super::help::announce(&t!(
                        "{name}: {value}",
                        name = src.name(index),
                        value = text
                    ));
                }
                TypedValue::Blank => value_text.set_value(&current),
                TypedValue::NoSuchValue => {
                    value_text.set_value(&current);
                    super::help::announce(&t!(
                        "No value called {typed}. Still {current}.",
                        typed = typed.trim(),
                        current = current
                    ));
                }
                TypedValue::Unsupported => {
                    value_text.set_value(&current);
                    super::help::announce(&t!(
                        "This plugin does not accept typed values. Still {current}.",
                        current = current
                    ));
                }
            }
        }
    };
    {
        let apply_typed = apply_typed.clone();
        value_text.clone().on_kill_focus(move |event| {
            apply_typed();
            event.skip(true);
        });
    }
    {
        let apply_typed = apply_typed.clone();
        value_text.clone().on_text_enter(move |event| {
            apply_typed();
            // Consumed, or the Return reaches the native EDIT and it beeps —
            // see the same call in `super::chat::build` for why.
            event.skip(false);
        });
    }

    // Ctrl+Tab anywhere in the dialog cycles parameters; Escape closes it.
    // (The value slider handles both itself, above.)
    let nav_keys = {
        let cycle_param = cycle_param.clone();
        move |event: WindowEventData| match super::key_of(&event) {
            Some((WXK_ESCAPE, _)) => dialog.end_modal(ID_OK),
            Some((WXK_TAB, true)) => cycle_param(!event_shift(&event)),
            _ => event.skip(true),
        }
    };
    {
        let nav_keys = nav_keys.clone();
        filter_box.clone().on_key_down(nav_keys);
    }
    {
        let nav_keys = nav_keys.clone();
        param_choice.clone().on_key_down(nav_keys);
    }
    {
        let nav_keys = nav_keys.clone();
        unnamed_check.clone().on_key_down(nav_keys);
    }
    {
        let nav_keys = nav_keys.clone();
        value_text.clone().on_key_down(nav_keys);
    }

    {
        close.on_click(move |_| dialog.end_modal(ID_OK));
    }

    // A plugin with one parameter has nothing to filter and nothing to choose
    // between, so the dialog opens on the control that changes it.
    if visible.borrow().len() == 1 {
        value_slider.set_focus();
    }

    dialog.show_modal();
    // Remove the slider's UIA provider while its window still exists.
    announcer.uninstall();
    dialog.destroy();

    // Persist whatever the user changed.
    fx::snapshot_slot(app, target, slot);
}

/// Whether Shift was held for a key event (for Ctrl+Shift+Tab).
fn event_shift(event: &WindowEventData) -> bool {
    if let WindowEventData::Keyboard(kb) = event {
        kb.shift_down()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeParam {
        name: String,
        value: RefCell<f32>,
        automatable: bool,
        /// Formats value into text; empty string means "no display".
        format: fn(f32) -> String,
        label: String,
        /// Parses display text into a normalized value; `None` means the plugin
        /// does not support string-to-parameter conversion.
        parse: Option<fn(&str) -> Option<f32>>,
        /// Steps above the lowest position; 0 for a continuous parameter, or
        /// for a format that does not say (VST2).
        steps: u32,
    }

    struct FakeParams {
        params: Vec<FakeParam>,
    }

    impl ParamSource for FakeParams {
        fn count(&self) -> i32 {
            self.params.len() as i32
        }
        fn name(&self, index: i32) -> String {
            self.params[index as usize].name.clone()
        }
        fn get(&self, index: i32) -> f32 {
            *self.params[index as usize].value.borrow()
        }
        fn set(&self, index: i32, value: f32) {
            *self.params[index as usize].value.borrow_mut() = value.clamp(0.0, 1.0);
        }
        fn display(&self, index: i32) -> String {
            let p = &self.params[index as usize];
            (p.format)(*p.value.borrow())
        }
        fn label(&self, index: i32) -> String {
            self.params[index as usize].label.clone()
        }
        fn automatable(&self, index: i32) -> bool {
            self.params[index as usize].automatable
        }
        fn set_from_string(&self, index: i32, text: &str) -> bool {
            let p = &self.params[index as usize];
            match p.parse.and_then(|f| f(text)) {
                Some(v) => {
                    *p.value.borrow_mut() = v.clamp(0.0, 1.0);
                    true
                }
                None => false,
            }
        }
        fn steps(&self, index: i32) -> u32 {
            self.params[index as usize].steps
        }
        fn display_at(&self, index: i32, value: f32) -> Option<String> {
            let p = &self.params[index as usize];
            // A format that reports steps can also format a value it has not
            // been set to; one that reports none can do neither.
            (p.steps > 0).then(|| (p.format)(value))
        }
    }

    /// One stepped parameter whose every position has a name, which is the
    /// whole interface of a plugin that offers a list of presets and nothing
    /// else.
    const VOICES: [&str; 5] = ["Normal", "Child", "Chipmunk", "Robot", "Dark Lord"];

    fn voices() -> FakeParams {
        FakeParams {
            params: vec![FakeParam {
                name: "Voice".into(),
                value: RefCell::new(0.0),
                automatable: true,
                // VST3's own rule for a stepped parameter: `min(steps, v * (steps + 1))`.
                format: |v| VOICES[((v * VOICES.len() as f32) as usize).min(VOICES.len() - 1)].to_string(),
                label: String::new(),
                parse: None,
                steps: VOICES.len() as u32 - 1,
            }],
        }
    }

    fn sample() -> FakeParams {
        FakeParams {
            params: vec![
                // Continuous, formatted in dB.
                FakeParam {
                    name: "Gain".into(),
                    value: RefCell::new(0.5),
                    automatable: true,
                    format: |v| format!("{:.1}", v * 24.0 - 12.0),
                    label: "dB".into(),
                    // Inverse of the dB mapping above (-12..+12 dB → 0..1).
                    parse: Some(|s| s.trim().parse::<f32>().ok().map(|db| (db + 12.0) / 24.0)),
                    steps: 0,
                },
                // Discrete 4-state, text changes only at quarter boundaries —
                // and, as a VST2 plugin's would, without saying it is stepped.
                FakeParam {
                    name: "Mode".into(),
                    value: RefCell::new(0.0),
                    automatable: true,
                    format: |v| {
                        ["Off", "Low", "Mid", "High"][((v * 3.0).round() as usize).min(3)]
                            .to_string()
                    },
                    label: String::new(),
                    parse: None,
                    steps: 0,
                },
                // Unnamed / non-automatable.
                FakeParam {
                    name: "3".into(),
                    value: RefCell::new(0.0),
                    automatable: true,
                    format: |_| String::new(),
                    label: String::new(),
                    parse: None,
                    steps: 0,
                },
                FakeParam {
                    name: "Hidden".into(),
                    value: RefCell::new(0.0),
                    automatable: false,
                    format: |_| String::new(),
                    label: String::new(),
                    parse: None,
                    steps: 0,
                },
            ],
        }
    }

    #[test]
    fn formatted_value_uses_display_and_label_then_percent() {
        let p = sample();
        assert_eq!(formatted_value(&p, 0), "0.0 dB");
        // Param 2 has no display text → percentage fallback.
        assert_eq!(formatted_value(&p, 2), "0%");
    }

    #[test]
    fn apply_typed_value_uses_conversion_or_reports_unsupported() {
        let p = sample();
        // Gain supports conversion: "-6" dB → 0.25 normalized, and re-displays.
        let applied = apply_typed_value(&p, 0, "-6");
        assert_eq!(applied, TypedValue::Applied(0.25));
        assert_eq!(formatted_value(&p, 0), "-6.0 dB");
        // Blank input is ignored — distinct from a refusal, so the dialog can
        // stay quiet for one and speak up for the other.
        assert_eq!(apply_typed_value(&p, 0, "   "), TypedValue::Blank);
        // Mode does not support conversion; the value is left alone.
        assert_eq!(apply_typed_value(&p, 1, "High"), TypedValue::Unsupported);
        assert_eq!(*p.params[1].value.borrow(), 0.0);
    }

    #[test]
    fn is_named_rejects_blank_and_numeric() {
        assert!(is_named("Gain"));
        assert!(!is_named(""));
        assert!(!is_named("   "));
        assert!(!is_named("42"));
    }

    #[test]
    fn filter_hides_unnamed_and_non_automatable() {
        let p = sample();
        let visible = filter_params(&p, "", false);
        // Gain and Mode only (index 2 unnamed, index 3 not automatable).
        assert_eq!(visible, vec![0, 1]);
        // Including unnamed adds index 2 but never the non-automatable one.
        assert_eq!(filter_params(&p, "", true), vec![0, 1, 2]);
        // Substring filter.
        assert_eq!(filter_params(&p, "mod", false), vec![1]);
    }

    #[test]
    fn small_step_moves_continuous_param_to_next_distinct_value() {
        let p = sample();
        let v = step_param(&p, 0, SMALL_STEP);
        // Moves up to the next value whose displayed dB differs.
        assert!(v > 0.5 && v < 0.55, "got {v}");
        assert_ne!(formatted_value(&p, 0), "0.0 dB");
    }

    #[test]
    fn step_snaps_discrete_param_by_one_detent() {
        let p = sample();
        // Mode starts at "Off" (0.0). One small up-step must reach "Low".
        assert_eq!(formatted_value(&p, 1), "Off");
        step_param(&p, 1, SMALL_STEP);
        assert_eq!(formatted_value(&p, 1), "Low");
    }

    #[test]
    fn a_stepped_parameter_gets_one_slider_position_per_value() {
        // Five voices are four steps, and so a slider that runs 0 to 4.
        assert_eq!(slider_range(4), 4);
        // A switch is a slider with two ends and nothing between them.
        assert_eq!(slider_range(1), 1);
        // Continuous, and anything too finely stepped to be worth a position
        // each, keep the full sweep.
        assert_eq!(slider_range(0), SLIDER_RANGE);
        assert_eq!(slider_range(100_000), SLIDER_RANGE);
        assert!(!is_stepped(0) && is_stepped(1) && is_stepped(1000) && !is_stepped(1001));
    }

    #[test]
    fn every_slider_position_is_the_value_it_names() {
        // Out and back for every voice of a list as long as the real one: a
        // position that came back as its neighbour would announce one voice
        // and select another.
        let range = 52;
        for position in 0..=range {
            let value = slider_value(position, range);
            assert_eq!(slider_position(value, range), position);
            // And VST3's own mapping agrees which step that is.
            let step = ((value as f64 * (range + 1) as f64) as i32).min(range);
            assert_eq!(step, position, "position {position} of {range}");
        }
    }

    #[test]
    fn each_arrow_moves_a_stepped_parameter_exactly_one_value() {
        let p = voices();
        for expected in &VOICES[1..] {
            key_param(&p, 0, WXK_UP).expect("a movement key");
            assert_eq!(formatted_value(&p, 0), *expected);
        }
        for expected in VOICES[..VOICES.len() - 1].iter().rev() {
            key_param(&p, 0, WXK_DOWN).expect("a movement key");
            assert_eq!(formatted_value(&p, 0), *expected);
        }
    }

    #[test]
    fn left_and_right_move_the_parameter_too() {
        // These used to fall through to the native trackbar, which moved the
        // thumb and told the plugin nothing.
        let p = voices();
        key_param(&p, 0, WXK_RIGHT);
        assert_eq!(formatted_value(&p, 0), "Child");
        key_param(&p, 0, WXK_LEFT);
        assert_eq!(formatted_value(&p, 0), "Normal");

        let continuous = sample();
        let up = key_param(&continuous, 0, WXK_RIGHT).expect("a movement key");
        assert!(up > 0.5, "got {up}");
        let down = key_param(&continuous, 0, WXK_LEFT).expect("a movement key");
        assert!(down < up, "got {down}");
    }

    #[test]
    fn home_and_end_reach_both_ends_and_a_key_at_the_end_still_answers() {
        let p = voices();
        assert_eq!(key_param(&p, 0, WXK_HOME), Some(1.0));
        assert_eq!(formatted_value(&p, 0), "Dark Lord");
        // Already at the top: nothing moves, but it is still a movement key,
        // so the dialog still announces where the parameter is.
        assert_eq!(key_param(&p, 0, WXK_UP), Some(1.0));
        assert_eq!(key_param(&p, 0, WXK_END), Some(0.0));
        assert_eq!(formatted_value(&p, 0), "Normal");
        // A page is never nothing, even on a list this short.
        key_param(&p, 0, WXK_PAGEUP);
        assert_eq!(formatted_value(&p, 0), "Child");
    }

    #[test]
    fn a_key_that_does_not_move_a_slider_is_left_for_the_dialog() {
        assert_eq!(key_param(&voices(), 0, WXK_TAB), None);
        assert_eq!(key_param(&sample(), 0, WXK_TAB), None);
    }

    #[test]
    fn a_stepped_value_can_be_typed_by_name() {
        let p = voices();
        // The plugin cannot parse text, so this is the dialog reading every
        // step's name and finding the one that was typed.
        assert!(matches!(
            apply_typed_value(&p, 0, "  chipmunk "),
            TypedValue::Applied(_)
        ));
        assert_eq!(formatted_value(&p, 0), "Chipmunk");
        // A prefix only one value starts with is enough...
        assert!(matches!(apply_typed_value(&p, 0, "dark"), TypedValue::Applied(_)));
        assert_eq!(formatted_value(&p, 0), "Dark Lord");
        // ...and one that two start with is not, and changes nothing.
        assert_eq!(apply_typed_value(&p, 0, "chi"), TypedValue::NoSuchValue);
        assert_eq!(apply_typed_value(&p, 0, "Gremlin"), TypedValue::NoSuchValue);
        assert_eq!(formatted_value(&p, 0), "Dark Lord");
        // A whole name wins over being a prefix of another.
        assert!(matches!(apply_typed_value(&p, 0, "Child"), TypedValue::Applied(_)));
        assert_eq!(formatted_value(&p, 0), "Child");
    }

    #[test]
    fn set_param_clamps() {
        let p = sample();
        assert_eq!(set_param(&p, 0, 2.0), 1.0);
        assert_eq!(set_param(&p, 0, -1.0), 0.0);
    }
}
