//! Buses tab: manage the global mixing buses that sources send to, and edit
//! the FX chain on the selected bus (or on the master output).
//!
//! The bus list has a pinned first row, "Master output", which stands for the
//! master FX chain; the remaining rows are the real buses. So list row 0 maps
//! to `ChainTarget::Master` and row `n` to `ChainTarget::Bus(n - 1)`. Volume,
//! mute, rename, remove, and reorder apply to real buses only (in the Home
//! mixer for volume/mute); row 0 only offers an FX chain.
//!
//! The chain editor under the list is [`super::chain_ui`], the same one the
//! Scenes tab shows for a source. This file owns the bus list and says which
//! chain is selected; the bus list needs no placeholder row, since "Master
//! output" is always there.

use crate::t;
use super::chain_ui::{self, ChainHost, ChainWidgets};
use super::fx::{self, ChainTarget};
use super::{App, WXK_DELETE, WXK_DOWN, WXK_UP, show_error};
use crate::state::{ListEdit, move_down, move_up};
use std::rc::Rc;
use wxdragon::prelude::*;

/// Rows before a bus in the list (the pinned "Master output" row).
const MASTER_ROWS: u32 = 1;

pub fn build(app: &Rc<App>, panel: &Panel) -> (ListBox, ChainWidgets) {
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    // --- Buses ---
    let label = StaticText::builder(panel).with_label(&t!("Buses")).build();
    let bus_list = ListBox::builder(panel).build();
    super::native_acc::install(&bus_list, &t!("Buses"));
    super::help::tag(
        &bus_list,
        "tab.buses.busList",
        "Buses list (row 0 is the master output)",
    );
    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let add = Button::builder(panel).with_label(&t!("&Add bus")).build();
    let rename = Button::builder(panel).with_label(&t!("&Rename bus")).build();
    let remove = Button::builder(panel).with_label(&t!("Remove bus")).build();
    let up = Button::builder(panel).with_label(&t!("Move up")).build();
    let down = Button::builder(panel).with_label(&t!("Move down")).build();
    super::help::tag(&add, "tab.buses.busAdd", "Add bus button");
    super::help::tag(&rename, "tab.buses.busRename", "Rename bus button");
    super::help::tag(&remove, "tab.buses.busRemove", "Remove bus button");
    super::help::tag(&up, "tab.buses.busUp", "Move bus up button");
    super::help::tag(&down, "tab.buses.busDown", "Move bus down button");
    for b in [&add, &rename, &remove, &up, &down] {
        buttons.add(b, 0, SizerFlag::All, 4);
    }

    sizer.add(&label, 0, SizerFlag::All, 4);
    sizer.add(&bus_list, 1, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add_sizer(&buttons, 0, SizerFlag::Expand, 0);

    // --- FX chain for the selected bus ---
    let chain = chain_ui::build(app, panel, &sizer, ChainHost::Buses);
    panel.set_sizer(sizer, true);

    // Selecting a bus refreshes the FX list.
    {
        let app = app.clone();
        bus_list
            .clone()
            .on_selection_changed(move |_| refresh_fx_list(&app));
    }

    // Bus buttons.
    {
        let app = app.clone();
        add.on_click(move |_| add_bus(&app));
    }
    {
        let app = app.clone();
        let list = bus_list;
        rename.on_click(move |_| rename_bus(&app, &list));
    }
    {
        let app = app.clone();
        let list = bus_list;
        remove.on_click(move |_| remove_bus(&app, &list));
    }
    {
        let app = app.clone();
        let list = bus_list;
        up.on_click(move |_| move_bus(&app, &list, true));
    }
    {
        let app = app.clone();
        let list = bus_list;
        down.on_click(move |_| move_bus(&app, &list, false));
    }
    {
        let app = app.clone();
        let list = bus_list;
        bus_list
            .clone()
            .on_key_down(move |event| match super::key_of(&event) {
                Some((WXK_UP, true)) => move_bus(&app, &list, true),
                Some((WXK_DOWN, true)) => move_bus(&app, &list, false),
                Some((WXK_DELETE, _)) => remove_bus(&app, &list),
                _ => event.skip(true),
            });
    }

    (bus_list, chain)
}

/// The chain the given bus-list row targets.
fn target_for(row: u32) -> ChainTarget {
    if row < MASTER_ROWS {
        ChainTarget::Master
    } else {
        ChainTarget::Bus((row - MASTER_ROWS) as usize)
    }
}

/// The bus-list row currently selected (defaults to Master).
pub(super) fn selected_target(app: &Rc<App>) -> ChainTarget {
    app.widgets(|w| target_for(w.bus_list.get_selection().unwrap_or(0)))
        .unwrap_or(ChainTarget::Master)
}

/// Save, refresh both lists, re-sync the engine. Sources go in the same
/// routing command as the buses because their send targets are bus indices,
/// which a bus edit can shift.
fn after_bus_edit(app: &Rc<App>) {
    app.save_config();
    refresh_bus_list(app);
    refresh_fx_list(app);
    app.sync_engine_routing();
    super::home::rebuild_mixer(app);
}

pub fn refresh_bus_list(app: &Rc<App>) {
    app.widgets(|w| {
        let config = app.config.borrow();
        let selected = w.bus_list.get_selection();
        let labels: Vec<String> = std::iter::once(t!("Master output"))
            .chain(config.buses.buses.iter().map(|bus| bus.name.clone()))
            .collect();
        // The placeholder is unreachable: the pinned Master row means `labels`
        // is never empty.
        if super::list::sync(&w.bus_list, &labels, "") == super::list::Synced::Kept {
            return;
        }
        let target = selected.filter(|&i| i < w.bus_list.get_count());
        if let Some(index) = target {
            w.bus_list.set_selection(index, true);
        } else {
            w.bus_list.set_selection(0, true);
        }
    });
}

/// Refreshes the FX list to show the selected bus's chain.
pub fn refresh_fx_list(app: &Rc<App>) {
    chain_ui::refresh(app, ChainHost::Buses);
}

fn add_bus(app: &Rc<App>) {
    let Some(frame) = app.widgets(|w| w.frame) else {
        return;
    };
    let dialog = TextEntryDialog::builder(&frame, &t!("Bus name:"), &t!("Add bus")).build();
    if dialog.show_modal() == ID_OK
        && let Some(name) = dialog.get_value()
    {
        let result = app.config.borrow_mut().add_bus(&name);
        match result {
            ListEdit::Changed => {
                fx::on_bus_added(app);
                after_bus_edit(app);
                app.widgets(|w| {
                    let count = w.bus_list.get_count();
                    if count > 0 {
                        w.bus_list.set_selection(count - 1, true);
                    }
                });
                refresh_fx_list(app);
            }
            ListEdit::Unchanged if !name.trim().is_empty() => {
                show_error(&frame, &t!("Add bus"), &t!("A bus with that name already exists."));
            }
            ListEdit::Unchanged => {}
        }
    }
    dialog.destroy();
}

fn rename_bus(app: &Rc<App>, list: &ListBox) {
    let ChainTarget::Bus(index) = target_for(list.get_selection().unwrap_or(0)) else {
        return;
    };
    let Some(frame) = app.widgets(|w| w.frame) else {
        return;
    };
    let current = {
        let config = app.config.borrow();
        let Some(bus) = config.buses.buses.get(index) else {
            return;
        };
        bus.name.clone()
    };
    let dialog = TextEntryDialog::builder(&frame, &t!("New bus name:"), &t!("Rename bus"))
        .with_default_value(&current)
        .build();
    if dialog.show_modal() == ID_OK
        && let Some(name) = dialog.get_value()
    {
        let result = app.config.borrow_mut().rename_bus(index, &name);
        match result {
            ListEdit::Changed => after_bus_edit(app),
            ListEdit::Unchanged if !name.trim().is_empty() && name.trim() != current => {
                show_error(&frame, &t!("Rename bus"), &t!("A bus with that name already exists."));
            }
            ListEdit::Unchanged => {}
        }
    }
    dialog.destroy();
}

fn remove_bus(app: &Rc<App>, list: &ListBox) {
    let ChainTarget::Bus(index) = target_for(list.get_selection().unwrap_or(0)) else {
        return;
    };
    // Closing the editors snapshots each open plugin's state back into its
    // config slot, addressed by bus index — so it has to happen while the
    // config still agrees with `app.fx` about what lives at which index.
    // Deleting first would write the snapshot into whichever bus shifted down
    // into this slot. (`fx::remove_plugin` closes first for the same reason.)
    super::fx_editor::close_all(app);
    if app.config.borrow_mut().delete_bus(index) == ListEdit::Changed {
        fx::on_bus_removed(app, index);
        after_bus_edit(app);
    }
}

fn move_bus(app: &Rc<App>, list: &ListBox, towards_start: bool) {
    let ChainTarget::Bus(index) = target_for(list.get_selection().unwrap_or(0)) else {
        return;
    };
    // Snapshot open editors before the swap, for the same reason as
    // `remove_bus`: the snapshot is written back by bus index.
    super::fx_editor::close_all(app);
    let result = {
        let mut config = app.config.borrow_mut();
        if towards_start {
            move_up(&mut config.buses.buses, index)
        } else {
            move_down(&mut config.buses.buses, index)
        }
    };
    if result == ListEdit::Changed {
        let other = if towards_start { index - 1 } else { index + 1 };
        fx::on_bus_moved(app, index, other);
        after_bus_edit(app);
        app.widgets(|w| w.bus_list.set_selection((other as u32) + MASTER_ROWS, true));
        refresh_fx_list(app);
    }
}
