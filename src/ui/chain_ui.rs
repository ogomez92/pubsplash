//! The FX chain editor: the list of plugins on one chain, and the controls
//! that add to it, reorder it, bypass, edit and save it.
//!
//! Built twice, by [`build`]: on the Scenes tab for the selected source's
//! chain, and on the Buses tab for the selected bus's (or the master's). The two
//! are the same controls doing the same things to a different [`ChainTarget`],
//! and [`ChainHost`] is the whole of the difference — which widgets, which
//! chain they currently show, and the handful of words that name it.
//!
//! Nothing here caches a target. Every handler asks its host which chain is
//! selected *now*, because the selection that decides it lives in another list
//! and moves without telling this one.
//!
//! The `help::tag` ids and the `t!` strings are spelled out per host rather
//! than assembled, in both cases because a dev tool finds them by scanning for
//! literal call sites (`gen-help`, `gen-po`). The Buses labels also keep the
//! mnemonics they have always had, and the Scenes ones carry none — see
//! `agents.md`.

use super::fx::{self, ChainTarget};
use super::{App, WXK_DELETE, WXK_DOWN, WXK_UP, show_error, show_info};
use crate::t;
use crate::tn;
use std::rc::Rc;
use wxdragon::prelude::*;

/// Which of the two chain editors a handler belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainHost {
    /// The Scenes tab: the chain of the source selected in its Sources list.
    Sources,
    /// The Buses tab: the chain of the bus selected in its list, or the
    /// master's when the pinned first row is.
    Buses,
}

/// Which chain an editor is showing, named by something that outlives the
/// edits that shift an index: the source's key, the bus's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shown {
    Source(fx::SourceKey),
    Bus(String),
    Master,
}

/// What each editor last showed, so [`refresh`] can tell "the same chain,
/// edited" from "a different chain".
#[derive(Default)]
pub struct ShownChains {
    sources: Option<Shown>,
    buses: Option<Shown>,
}

fn shown_for(app: &Rc<App>, target: ChainTarget) -> Option<Shown> {
    match target {
        ChainTarget::Source { scene, source } => fx::key_of(app, scene, source).map(Shown::Source),
        ChainTarget::Bus(i) => app
            .config
            .borrow()
            .buses
            .buses
            .get(i)
            .map(|bus| Shown::Bus(bus.name.clone())),
        ChainTarget::Master => Some(Shown::Master),
    }
}

/// The two widgets of an editor that are read back after it is built.
#[derive(Clone, Copy)]
pub struct ChainWidgets {
    pub list: ListBox,
    /// Reflects (and toggles) the selected plugin's bypass state.
    pub bypass: CheckBox,
}

impl ChainHost {
    /// The chain this editor is showing, or `None` when nothing that owns one
    /// is selected — a scene with no sources.
    fn target(self, app: &Rc<App>) -> Option<ChainTarget> {
        match self {
            ChainHost::Sources => super::scenes::selected_source_target(app),
            ChainHost::Buses => Some(super::buses::selected_target(app)),
        }
    }

    fn widgets(self, app: &Rc<App>) -> Option<ChainWidgets> {
        app.widgets(|w| match self {
            ChainHost::Sources => ChainWidgets {
                list: w.source_fx_list,
                bypass: w.source_fx_bypass,
            },
            ChainHost::Buses => ChainWidgets {
                list: w.fx_list,
                bypass: w.fx_bypass,
            },
        })
    }

    fn list_label(self) -> String {
        match self {
            ChainHost::Sources => t!("Effects on selected source"),
            ChainHost::Buses => t!("Effects on selected bus"),
        }
    }

    /// What a chain edit owes the rest of its tab, beyond this editor's own
    /// list.
    fn after_edit(self, app: &Rc<App>) {
        match self {
            ChainHost::Sources => {}
            ChainHost::Buses => {
                super::buses::refresh_bus_list(app);
                super::home::rebuild_mixer(app);
            }
        }
    }
}

/// Shown when a list has nothing in it. See [`super::list`].
fn no_plugins() -> String {
    t!("No plugins")
}
fn no_saved_chains() -> String {
    t!("No saved chains")
}

/// The chain a handler should act on, or — having told the user why not —
/// `None`. Only the Scenes tab can have nothing selected.
fn target_or_explain(app: &Rc<App>, host: ChainHost, title: &str) -> Option<ChainTarget> {
    let target = host.target(app);
    if target.is_none()
        && let Some(frame) = app.widgets(|w| w.frame)
    {
        show_info(
            &frame,
            title,
            &t!("Select a source first. Effects belong to the source selected in the list above."),
        );
    }
    target
}

/// Adds the chain editor's controls to `sizer` and wires them up.
pub fn build(app: &Rc<App>, panel: &Panel, sizer: &BoxSizer, host: ChainHost) -> ChainWidgets {
    let fx_label = StaticText::builder(panel)
        .with_label(&host.list_label())
        .build();
    let fx_list = ListBox::builder(panel).build();
    super::native_acc::install(&fx_list, &host.list_label());

    let fx_buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let (add_label, bypass_label, edit_label, open_label) = match host {
        ChainHost::Sources => (
            t!("Add plugin"),
            t!("Bypass"),
            t!("Edit parameters"),
            t!("Open interface"),
        ),
        ChainHost::Buses => (
            t!("Add &plugin"),
            t!("&Bypass"),
            t!("&Edit parameters"),
            t!("Open &interface"),
        ),
    };
    let fx_add = Button::builder(panel).with_label(&add_label).build();
    let fx_remove = Button::builder(panel).with_label(&t!("Remove plugin")).build();
    let fx_up = Button::builder(panel).with_label(&t!("Move plugin up")).build();
    let fx_down = Button::builder(panel)
        .with_label(&t!("Move plugin down"))
        .build();
    let fx_bypass = CheckBox::builder(panel).with_label(&bypass_label).build();
    super::set_accessible_name(&fx_bypass, &t!("Bypass selected plugin"));
    let fx_edit = Button::builder(panel).with_label(&edit_label).build();
    let fx_open = Button::builder(panel).with_label(&open_label).build();
    for b in [&fx_add, &fx_remove, &fx_up, &fx_down] {
        fx_buttons.add(b, 0, SizerFlag::All, 4);
    }
    fx_buttons.add(
        &fx_bypass,
        0,
        SizerFlag::AlignCenterVertical | SizerFlag::All,
        4,
    );
    for b in [&fx_edit, &fx_open] {
        fx_buttons.add(b, 0, SizerFlag::All, 4);
    }

    // Chain library row.
    let lib_buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let save_chain = Button::builder(panel).with_label(&t!("Save chain...")).build();
    let load_chain = Button::builder(panel).with_label(&t!("Load chain...")).build();
    let import_chain = Button::builder(panel)
        .with_label(&t!("Import chain..."))
        .build();
    let export_chain = Button::builder(panel)
        .with_label(&t!("Export chain..."))
        .build();
    for b in [&save_chain, &load_chain, &import_chain, &export_chain] {
        lib_buttons.add(b, 0, SizerFlag::All, 4);
    }

    match host {
        ChainHost::Sources => {
            super::help::tag(
                &fx_list,
                "tab.scenes.fxList",
                "Effects chain on the selected source",
            );
            super::help::tag(&fx_add, "tab.scenes.fxAdd", "Add plugin to source button");
            super::help::tag(
                &fx_remove,
                "tab.scenes.fxRemove",
                "Remove plugin from source button",
            );
            super::help::tag(&fx_up, "tab.scenes.fxUp", "Move source plugin up button");
            super::help::tag(
                &fx_down,
                "tab.scenes.fxDown",
                "Move source plugin down button",
            );
            super::help::tag(
                &fx_bypass,
                "tab.scenes.fxBypass",
                "Bypass selected source plugin checkbox",
            );
            super::help::tag(
                &fx_edit,
                "tab.scenes.fxEditParams",
                "Edit source plugin parameters button",
            );
            super::help::tag(
                &fx_open,
                "tab.scenes.fxOpenInterface",
                "Open source plugin interface button",
            );
            super::help::tag(
                &save_chain,
                "tab.scenes.chainSave",
                "Save source FX chain to library button",
            );
            super::help::tag(
                &load_chain,
                "tab.scenes.chainLoad",
                "Load FX chain from library onto source button",
            );
            super::help::tag(
                &import_chain,
                "tab.scenes.chainImport",
                "Import FX chain file for source button",
            );
            super::help::tag(
                &export_chain,
                "tab.scenes.chainExport",
                "Export source FX chain file button",
            );
        }
        ChainHost::Buses => {
            super::help::tag(
                &fx_list,
                "tab.buses.fxList",
                "Effects chain on the selected bus",
            );
            super::help::tag(&fx_add, "tab.buses.fxAdd", "Add plugin to chain button");
            super::help::tag(
                &fx_remove,
                "tab.buses.fxRemove",
                "Remove plugin from chain button",
            );
            super::help::tag(&fx_up, "tab.buses.fxUp", "Move plugin up in chain button");
            super::help::tag(
                &fx_down,
                "tab.buses.fxDown",
                "Move plugin down in chain button",
            );
            super::help::tag(
                &fx_bypass,
                "tab.buses.fxBypass",
                "Bypass selected plugin checkbox",
            );
            super::help::tag(
                &fx_edit,
                "tab.buses.fxEditParams",
                "Edit plugin parameters button",
            );
            super::help::tag(
                &fx_open,
                "tab.buses.fxOpenInterface",
                "Open plugin interface button",
            );
            super::help::tag(
                &save_chain,
                "tab.buses.chainSave",
                "Save FX chain to library button",
            );
            super::help::tag(
                &load_chain,
                "tab.buses.chainLoad",
                "Load FX chain from library button",
            );
            super::help::tag(
                &import_chain,
                "tab.buses.chainImport",
                "Import FX chain file button",
            );
            super::help::tag(
                &export_chain,
                "tab.buses.chainExport",
                "Export FX chain file button",
            );
        }
    }

    sizer.add(&fx_label, 0, SizerFlag::All, 4);
    sizer.add(&fx_list, 1, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add_sizer(&fx_buttons, 0, SizerFlag::Expand, 0);
    sizer.add_sizer(&lib_buttons, 0, SizerFlag::Expand, 0);

    // FX chain buttons.
    {
        let app = app.clone();
        fx_add.on_click(move |_| add_plugin(&app, host));
    }
    {
        let app = app.clone();
        fx_remove.on_click(move |_| remove_plugin(&app, host));
    }
    {
        let app = app.clone();
        fx_up.on_click(move |_| move_plugin(&app, host, true));
    }
    {
        let app = app.clone();
        fx_down.on_click(move |_| move_plugin(&app, host, false));
    }
    {
        let app = app.clone();
        fx_bypass
            .clone()
            .on_toggled(move |_| set_bypass(&app, host, fx_bypass.get_value()));
    }
    // Keep the Bypass checkbox showing the newly selected plugin's state.
    {
        let app = app.clone();
        fx_list
            .clone()
            .on_selection_changed(move |_| sync_bypass_check(&app, host));
    }
    {
        let app = app.clone();
        fx_edit.on_click(move |_| edit_parameters(&app, host));
    }
    {
        let app = app.clone();
        fx_open.on_click(move |_| open_interface(&app, host));
    }

    // Chain library buttons.
    {
        let app = app.clone();
        save_chain.on_click(move |_| save_chain_to_library(&app, host));
    }
    {
        let app = app.clone();
        load_chain.on_click(move |_| load_chain_from_library(&app, host));
    }
    {
        let app = app.clone();
        import_chain.on_click(move |_| import_chain_file(&app, host));
    }
    {
        let app = app.clone();
        export_chain.on_click(move |_| export_chain_file(&app, host));
    }
    {
        let app = app.clone();
        fx_list
            .clone()
            .on_key_down(move |event| match super::key_of(&event) {
                Some((WXK_UP, true)) => move_plugin(&app, host, true),
                Some((WXK_DOWN, true)) => move_plugin(&app, host, false),
                Some((WXK_DELETE, _)) => remove_plugin(&app, host),
                _ => event.skip(true),
            });
    }

    ChainWidgets {
        list: fx_list,
        bypass: fx_bypass,
    }
}

/// Refreshes both editors. For an edit made from somewhere that does not know
/// which of them, if either, is showing the chain it touched — the bypass box
/// on a plugin's own interface window.
pub fn refresh_all(app: &Rc<App>) {
    refresh(app, ChainHost::Sources);
    refresh(app, ChainHost::Buses);
}

/// Refreshes one editor's list to show the chain its host has selected.
///
/// **A selection belongs to the chain it was made on**, and is dropped when
/// the editor moves to another. Two things depend on that. The rows are
/// relabelled in place where the two chains are the same length, and a row
/// rewritten while selected is spoken whether or not its list has focus — so
/// without it, arrowing down the Sources list would read out a plugin from
/// each source's chain over the source's own name. And a selection carried
/// across would leave Bypass and Edit parameters aimed at whichever plugin
/// happened to sit at the same position in a chain the user never looked at.
pub fn refresh(app: &Rc<App>, host: ChainHost) {
    let target = host.target(app);
    let labels = match target {
        Some(target) => slot_labels(app, target),
        None => Vec::new(),
    };
    let Some(widgets) = host.widgets(app) else {
        return;
    };
    let shown = target.and_then(|target| shown_for(app, target));
    let moved = {
        let mut chains = app.shown_chains.borrow_mut();
        let last = match host {
            ChainHost::Sources => &mut chains.sources,
            ChainHost::Buses => &mut chains.buses,
        };
        let moved = *last != shown;
        *last = shown;
        moved
    };
    let previous = widgets.list.get_selection();
    if moved && let Some(row) = previous {
        widgets.list.set_selection(row, false);
    }
    if super::list::sync(&widgets.list, &labels, &no_plugins()) == super::list::Synced::Rebuilt
        && !moved
        && let Some(index) = previous.filter(|&i| i < widgets.list.get_count())
    {
        widgets.list.set_selection(index, true);
    }
    sync_bypass_check(app, host);
}

fn slot_labels(app: &Rc<App>, target: ChainTarget) -> Vec<String> {
    // Build the labels first, reading the chain in place rather than cloning
    // it: `FxSlotConfig` owns the plugin's serialized state, which can be
    // hundreds of kilobytes, and none of it is needed here.
    let count = fx::with_slots(app, target, |slots| slots.len()).unwrap_or(0);
    // Whether each instance actually loaded lives in `app.fx`, not the config,
    // and is gathered separately so neither borrow spans the other.
    let statuses: Vec<fx::SlotStatus> = (0..count)
        .map(|i| {
            let loaded = fx::instance_at(app, target, i).is_some();
            fx::slot_status(app, target, i, loaded)
        })
        .collect();
    fx::with_slots(app, target, |slots| {
        slots
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                let status = statuses
                    .get(i)
                    .copied()
                    .unwrap_or(fx::SlotStatus::NotInstalled);
                fx::slot_label(i, slot, status)
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default()
}

fn add_plugin(app: &Rc<App>, host: ChainHost) {
    let Some(target) = target_or_explain(app, host, &t!("Add plugin")) else {
        return;
    };
    let Some(frame) = app.widgets(|w| w.frame) else {
        return;
    };
    let choices: Vec<(String, crate::config::PluginRef)> = {
        let cache = app.plugins.borrow();
        cache
            .plugins
            .iter()
            .map(|p| {
                (
                    format!("{} ({})", p.name, p.format.display_name()),
                    crate::config::PluginRef::from_info(p),
                )
            })
            .collect()
    };
    if choices.is_empty() {
        show_info(
            &frame,
            &t!("Add plugin"),
            &t!("No compatible VST plugins are available. Scan for plugins in Preferences first."),
        );
        return;
    }
    let labels: Vec<&str> = choices.iter().map(|(name, _)| name.as_str()).collect();
    let dialog =
        SingleChoiceDialog::builder(&frame, &t!("Add which plugin?"), &t!("Add plugin"), &labels).build();
    super::native_acc::install_in_dialog(&dialog, &t!("Add which plugin?"));
    if dialog.show_modal() == ID_OK {
        let index = dialog.get_selection() as usize;
        if let Some((name, plugin)) = choices.get(index) {
            let outcome = fx::add_plugin(app, target, plugin.clone());
            host.after_edit(app);
            refresh(app, host);
            // Land on what was just added: it is the plugin the next press of
            // Edit parameters or Bypass is meant for.
            if let Some(widgets) = host.widgets(app) {
                let count = fx::with_slots(app, target, |slots| slots.len()).unwrap_or(0);
                if count > 0 {
                    widgets.list.set_selection(count as u32 - 1, true);
                    sync_bypass_check(app, host);
                }
            }
            if let Err(error) = outcome {
                let reason = match error {
                    fx::SlotError::NotInstalled => {
                        t!("it is no longer in the plugin cache. Rescan for plugins in Preferences.")
                            .to_string()
                    }
                    fx::SlotError::LoadFailed(reason) => reason,
                };
                show_error(
                    &frame,
                    &t!("Add plugin"),
                    &t!("{name} was added but could not be loaded, so it will not process audio: {reason}", name = name, reason = reason),
                );
            }
        }
    }
    dialog.destroy();
}

/// The selected plugin, as its host's chain and a slot in it. `None` when the
/// host has no chain selected, nothing is selected in the list, or the list is
/// showing its placeholder row.
fn selected_slot(app: &Rc<App>, host: ChainHost) -> Option<(ChainTarget, usize)> {
    let target = host.target(app)?;
    let widgets = host.widgets(app)?;
    let count = fx::with_slots(app, target, |slots| slots.len()).unwrap_or(0);
    Some((target, super::list::selection(&widgets.list, count)?))
}

fn remove_plugin(app: &Rc<App>, host: ChainHost) {
    let Some((target, slot)) = selected_slot(app, host) else {
        return;
    };
    fx::remove_plugin(app, target, slot);
    refresh(app, host);
}

fn move_plugin(app: &Rc<App>, host: ChainHost, towards_start: bool) {
    let Some((target, slot)) = selected_slot(app, host) else {
        return;
    };
    if fx::move_plugin(app, target, slot, towards_start) {
        refresh(app, host);
        let new_slot = if towards_start { slot - 1 } else { slot + 1 };
        if let Some(widgets) = host.widgets(app) {
            widgets.list.set_selection(new_slot as u32, true);
        }
    }
}

/// Applies the Bypass checkbox state to the selected plugin.
fn set_bypass(app: &Rc<App>, host: ChainHost, bypass: bool) {
    let Some((target, slot)) = selected_slot(app, host) else {
        return;
    };
    fx::set_bypass(app, target, slot, bypass);
    refresh(app, host);
}

/// Reflects the selected plugin's bypass state in the Bypass checkbox (and
/// disables it when no plugin is selected).
///
/// Writes only what has changed. The Scenes tab's editor is refreshed along
/// with its Sources list, which the application poll does every two seconds,
/// and a checkbox set to the value it already has is still a checkbox a screen
/// reader may be told about.
fn sync_bypass_check(app: &Rc<App>, host: ChainHost) {
    let Some(widgets) = host.widgets(app) else {
        return;
    };
    // Bound to the FX list's selection-changed event, so this runs on every
    // arrow key: project to the one flag it needs rather than cloning the
    // chain (and every plugin's serialized state with it).
    let bypassed = selected_slot(app, host).and_then(|(target, slot)| {
        fx::with_slots(app, target, |slots| {
            slots.get(slot).map(|s| s.bypass).unwrap_or(false)
        })
    });
    let (value, enabled) = match bypassed {
        Some(bypassed) => (bypassed, true),
        None => (false, false),
    };
    if widgets.bypass.get_value() != value {
        widgets.bypass.set_value(value);
    }
    if widgets.bypass.is_enabled() != enabled {
        widgets.bypass.enable(enabled);
    }
}

fn edit_parameters(app: &Rc<App>, host: ChainHost) {
    let Some((target, slot)) = selected_slot(app, host) else {
        return;
    };
    let Some(instance) = fx::instance_at(app, target, slot) else {
        if let Some(frame) = app.widgets(|w| w.frame) {
            show_info(
                &frame,
                &t!("Edit parameters"),
                &t!("This plugin is not loaded, so it has no parameters to edit."),
            );
        }
        return;
    };
    super::fx_params::edit_parameters(app, target, slot, instance);
}

fn open_interface(app: &Rc<App>, host: ChainHost) {
    let Some((target, slot)) = selected_slot(app, host) else {
        return;
    };
    let Some(instance) = fx::instance_at(app, target, slot) else {
        if let Some(frame) = app.widgets(|w| w.frame) {
            show_info(
                &frame,
                &t!("Open interface"),
                &t!("This plugin is not loaded, so it has no interface to open."),
            );
        }
        return;
    };
    super::fx_editor::open_editor(app, target, slot, instance);
}

// --- Chain library, import/export, and the missing-plugin dialog (S4) ---

/// A human label for a chain target, used in dialog messages and as the
/// default name for a chain saved from it.
fn target_name(app: &Rc<App>, target: ChainTarget) -> String {
    let config = app.config.borrow();
    match target {
        ChainTarget::Master => t!("Master output"),
        ChainTarget::Bus(i) => config
            .buses
            .buses
            .get(i)
            .map(|b| b.name.clone())
            .unwrap_or_default(),
        // What the mixer calls the source, never its identity name.
        ChainTarget::Source { scene, source } => config
            .scenes
            .scenes
            .get(scene)
            .and_then(|scene| {
                let ctx = app.name_context(&scene.sources);
                crate::source_name::strip_labels(&scene.sources, &ctx)
                    .into_iter()
                    .nth(source)
            })
            .unwrap_or_default(),
    }
}

fn save_chain_to_library(app: &Rc<App>, host: ChainHost) {
    let Some(target) = target_or_explain(app, host, &t!("Save chain")) else {
        return;
    };
    let Some(frame) = app.widgets(|w| w.frame) else {
        return;
    };
    // Capture current plugin state before saving.
    fx::snapshot_chain(app, target);
    let slots = fx::slots_for(app, target);
    if slots.is_empty() {
        show_info(
            &frame,
            &t!("Save chain"),
            &t!("This chain is empty; there is nothing to save."),
        );
        return;
    }
    let default_name = target_name(app, target);
    let dialog = TextEntryDialog::builder(&frame, &t!("Name for this chain:"), &t!("Save chain"))
        .with_default_value(&default_name)
        .build();
    if dialog.show_modal() == ID_OK
        && let Some(name) = dialog.get_value()
    {
        let name = name.trim().to_string();
        if !name.is_empty() {
            let mut library = app.chain_library.borrow_mut();
            let unique = crate::fx::unique_chain_name(&library.chains, &name);
            library.chains.push(crate::fx::NamedChain {
                name: unique.clone(),
                slots,
            });
            crate::fx::save_library(&library);
            drop(library);
            show_info(&frame, &t!("Save chain"), &t!("Saved chain \"{unique}\".", unique = unique));
        }
    }
    dialog.destroy();
}

fn load_chain_from_library(app: &Rc<App>, host: ChainHost) {
    let Some(target) = target_or_explain(app, host, &t!("Load chain")) else {
        return;
    };
    let Some(frame) = app.widgets(|w| w.frame) else {
        return;
    };
    let names: Vec<String> = app
        .chain_library
        .borrow()
        .chains
        .iter()
        .map(|c| c.name.clone())
        .collect();
    if names.is_empty() {
        show_info(&frame, &t!("Load chain"), &t!("There are no saved chains yet."));
        return;
    }
    let Some(chain_index) = pick_chain(app, &frame, &names) else {
        return;
    };
    let slots = app.chain_library.borrow().chains[chain_index].slots.clone();
    apply_chain(app, host, target, slots);
}

/// A small Load / Delete / Cancel picker over the saved chain names. Returns
/// the chosen chain index to load, or `None` if the user cancelled or only
/// deleted.
fn pick_chain(app: &Rc<App>, frame: &Frame, names: &[String]) -> Option<usize> {
    let dialog = Dialog::builder(frame, &t!("Load chain"))
        .with_style(DialogStyle::DefaultDialogStyle)
        .with_size(360, 300)
        .build();
    let panel = Panel::builder(&dialog).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    let label = StaticText::builder(&panel)
        .with_label(&t!("Saved chains"))
        .build();
    let list = ListBox::builder(&panel).build();
    super::native_acc::install(&list, &t!("Saved chains"));
    super::help::tag(&list, "dialog.loadChain.list", "Saved FX chains list");
    super::list::fill(&list, names, &no_saved_chains());
    if !names.is_empty() {
        list.set_selection(0, true);
    }
    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let load = super::ok_button(&panel, &t!("Load"));
    let delete = Button::builder(&panel).with_label(&t!("Delete")).build();
    // `ID_CANCEL` is what wx maps Escape to; without it Escape does nothing.
    let cancel = Button::builder(&panel)
        .with_id(ID_CANCEL)
        .with_label(&t!("Cancel"))
        .build();
    super::help::tag(&load, "dialog.loadChain.load", "Load selected chain button");
    super::help::tag(
        &delete,
        "dialog.loadChain.delete",
        "Delete selected chain button",
    );
    for b in [&load, &delete, &cancel] {
        buttons.add(b, 0, SizerFlag::All, 4);
    }
    sizer.add(&label, 0, SizerFlag::All, 4);
    sizer.add(&list, 1, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add_sizer(&buttons, 0, SizerFlag::AlignRight, 0);
    panel.set_sizer(sizer, true);
    let dialog_sizer = BoxSizer::builder(Orientation::Vertical).build();
    dialog_sizer.add(&panel, 1, SizerFlag::Expand, 0);
    dialog.set_sizer(dialog_sizer, true);

    // ID_OK carries "load the selection"; a delete re-runs the picker.
    {
        load.on_click(move |_| dialog.end_modal(ID_OK));
    }
    {
        cancel.on_click(move |_| dialog.end_modal(ID_CANCEL));
    }
    {
        let app = app.clone();
        delete.on_click(move |_| {
            let count = app.chain_library.borrow().chains.len();
            if let Some(index) = super::list::selection(&list, count) {
                {
                    let mut library = app.chain_library.borrow_mut();
                    library.chains.remove(index);
                    crate::fx::save_library(&library);
                }
                let names: Vec<String> = app
                    .chain_library
                    .borrow()
                    .chains
                    .iter()
                    .map(|chain| chain.name.clone())
                    .collect();
                super::list::fill(&list, &names, &no_saved_chains());
                if !names.is_empty() {
                    list.set_selection(0, true);
                }
            }
        });
    }

    let result = if dialog.show_modal() == ID_OK {
        // Deleting the last chain leaves only the placeholder row, which must
        // not come back as an index into `chain_library.chains`.
        let count = app.chain_library.borrow().chains.len();
        super::list::selection(&list, count)
    } else {
        None
    };
    dialog.destroy();
    result
}

fn import_chain_file(app: &Rc<App>, host: ChainHost) {
    let Some(frame) = app.widgets(|w| w.frame) else {
        return;
    };
    let dialog = FileDialog::builder(&frame)
        .with_message(&t!("Import FX chain"))
        .with_wildcard(&t!("Pubsplash FX chain (*.pubfx)|*.pubfx|All files (*.*)|*.*"))
        .with_style(FileDialogStyle::Open | FileDialogStyle::FileMustExist)
        .build();
    let path = if dialog.show_modal() == ID_OK {
        dialog.get_path()
    } else {
        None
    };
    if let Some(path) = path {
        match crate::fx::import_chain(std::path::Path::new(&path)) {
            Ok(chain) => {
                let slots = chain.slots.clone();
                {
                    let mut library = app.chain_library.borrow_mut();
                    let unique = crate::fx::unique_chain_name(&library.chains, &chain.name);
                    library.chains.push(crate::fx::NamedChain {
                        name: unique,
                        slots: chain.slots,
                    });
                    crate::fx::save_library(&library);
                }
                // Importing is worth doing with nothing selected — the chain
                // still joins the library — so only the offer to apply it
                // depends on there being somewhere to apply it to.
                let Some(target) = host.target(app) else {
                    show_info(
                        &frame,
                        &t!("Import chain"),
                        &t!("Chain imported and added to your library."),
                    );
                    return;
                };
                let question = match host {
                    ChainHost::Sources => t!(
                        "Chain imported and added to your library. Apply it to the selected source now?"
                    ),
                    ChainHost::Buses => t!(
                        "Chain imported and added to your library. Apply it to the selected bus now?"
                    ),
                };
                let apply = MessageDialog::builder(&frame, &question, &t!("Import chain"))
                    .with_style(MessageDialogStyle::YesNo | MessageDialogStyle::IconQuestion)
                    .build();
                if apply.show_modal() == ID_YES {
                    apply_chain(app, host, target, slots);
                }
            }
            Err(e) => show_error(&frame, &t!("Import chain"), &e),
        }
    }
}

fn export_chain_file(app: &Rc<App>, host: ChainHost) {
    let Some(target) = target_or_explain(app, host, &t!("Export chain")) else {
        return;
    };
    let Some(frame) = app.widgets(|w| w.frame) else {
        return;
    };
    fx::snapshot_chain(app, target);
    let slots = fx::slots_for(app, target);
    if slots.is_empty() {
        show_info(
            &frame,
            &t!("Export chain"),
            &t!("This chain is empty; there is nothing to export."),
        );
        return;
    }
    let name = target_name(app, target);
    let dialog = FileDialog::builder(&frame)
        .with_message(&t!("Export FX chain"))
        .with_default_file(&format!("{name}.pubfx"))
        .with_wildcard(&t!("Pubsplash FX chain (*.pubfx)|*.pubfx"))
        .with_style(FileDialogStyle::Save | FileDialogStyle::OverwritePrompt)
        .build();
    if dialog.show_modal() == ID_OK
        && let Some(mut path) = dialog.get_path()
    {
        if !path.to_lowercase().ends_with(".pubfx") {
            path.push_str(".pubfx");
        }
        let chain = crate::fx::NamedChain { name, slots };
        match crate::fx::export_chain(&chain, std::path::Path::new(&path)) {
            Ok(()) => show_info(&frame, &t!("Export chain"), &t!("Chain exported.")),
            Err(e) => show_error(&frame, &t!("Export chain"), &e),
        }
    }
}

/// Applies a set of slots to a target chain, first checking that every plugin
/// is installed. Missing plugins are reported; the user can apply with the
/// available subset or cancel.
fn apply_chain(
    app: &Rc<App>,
    host: ChainHost,
    target: ChainTarget,
    slots: Vec<crate::config::FxSlotConfig>,
) {
    let Some(frame) = app.widgets(|w| w.frame) else {
        return;
    };
    let resolution = {
        let cache = app.plugins.borrow();
        crate::fx::resolve_chain(&slots, &cache)
    };
    if !resolution.missing.is_empty() && !missing_plugin_dialog(&frame, &resolution) {
        return;
    }
    // Apply only the plugins that resolve on this machine.
    fx::replace_chain(app, target, resolution.valid);
    host.after_edit(app);
    refresh(app, host);
}

/// Shows which plugins in a chain are missing. Returns true if the user chose
/// to apply the chain with the available subset (only offered when at least
/// one plugin resolves).
fn missing_plugin_dialog(frame: &Frame, resolution: &crate::fx::ChainResolution) -> bool {
    let dialog = Dialog::builder(frame, &t!("Missing plugins"))
        .with_style(DialogStyle::DefaultDialogStyle)
        .with_size(420, 320)
        .build();
    let panel = Panel::builder(&dialog).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    let intro = StaticText::builder(&panel)
        .with_label(&format!(
            "{} of this chain's plugins are not installed on this machine:",
            resolution.missing.len()
        ))
        .build();
    let list = ListBox::builder(&panel).build();
    super::native_acc::install(&list, &t!("Missing plugins"));
    super::help::tag(&list, "dialog.missingPlugins.list", "Missing plugins list");
    for plugin in &resolution.missing {
        list.append(&plugin.display());
    }
    sizer.add(&intro, 0, SizerFlag::All, 8);
    sizer.add(&list, 1, SizerFlag::Expand | SizerFlag::All, 4);

    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let has_valid = !resolution.valid.is_empty();
    if has_valid {
        let apply = super::ok_button(
            &panel,
            &tn!(
                "Apply with {n} available plugin",
                "Apply with {n} available plugins",
                resolution.valid.len()
            ),
        );
        // `ID_CANCEL` is what wx maps Escape to; without it Escape does nothing.
        let cancel = Button::builder(&panel)
            .with_id(ID_CANCEL)
            .with_label(&t!("Cancel"))
            .build();
        super::help::tag(
            &apply,
            "dialog.missingPlugins.apply",
            "Apply chain with available plugins button",
        );
        buttons.add(&apply, 0, SizerFlag::All, 4);
        buttons.add(&cancel, 0, SizerFlag::All, 4);
        {
            apply.on_click(move |_| dialog.end_modal(ID_OK));
        }
        {
            cancel.on_click(move |_| dialog.end_modal(ID_CANCEL));
        }
    } else {
        let note = StaticText::builder(&panel)
            .with_label(&t!("None of the chain's plugins are available, so it cannot be applied."))
            .build();
        sizer.add(&note, 0, SizerFlag::All, 8);
        // Dismiss-only, so it carries `ID_CANCEL` despite the label: that id is
        // what wx maps Escape to, and it matches the `end_modal` below. Being the
        // default item as well, it answers Enter too.
        let ok = super::dismiss_button(&panel, &t!("OK"));
        buttons.add(&ok, 0, SizerFlag::All, 4);
        ok.on_click(move |_| dialog.end_modal(ID_CANCEL));
    }
    sizer.add_sizer(&buttons, 0, SizerFlag::AlignRight, 0);
    panel.set_sizer(sizer, true);
    let dialog_sizer = BoxSizer::builder(Orientation::Vertical).build();
    dialog_sizer.add(&panel, 1, SizerFlag::Expand, 0);
    dialog.set_sizer(dialog_sizer, true);

    let apply = dialog.show_modal() == ID_OK;
    dialog.destroy();
    apply
}
