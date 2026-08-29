//! The Media Scheduler source dialogs: the item list, and the editor for one
//! item.
//!
//! Split out of `scenes.rs`, where every other source dialog lives, because
//! this one is two dialogs and a preview rather than a page of fields. The
//! naming follows the keybind system's — [`super::scheduler`] is the runtime,
//! this is its interface.
//!
//! Two things here are deliberate.
//!
//! **The trigger controls are shown and hidden, not enabled and disabled**,
//! following `schedule_ui` and `connect_dialog`: a hidden control leaves the Tab
//! order entirely, where a disabled one stays in the accessibility tree while
//! being unreachable — so a screen-reader user tabs past two controls that
//! cannot be used before reaching the one that can.
//!
//! **The preview plays through the scheduler's own decoder**
//! ([`crate::media::decode::stream_file`]), not through
//! `audio::convert::decode_audio`. That makes the preview a real test: a file
//! that previews is a file the scheduler will play, on the same code path, and
//! a file that cannot be decoded says so here rather than at three in the
//! morning. It is bounded to [`PREVIEW_SECONDS`] because the decoder is a
//! streaming one and a scheduled file is not necessarily short — a whole-file
//! decode would be 115 MB for five minutes of audio, allocated on the UI thread.

use super::App;
use crate::config::{ScheduleItem, ScheduleTrigger, SchedulerSourceConfig, SourceKindConfig};
use crate::media::schedule::{self, INTERVALS};
use crate::t;
use std::cell::RefCell;
use std::rc::Rc;
use wxdragon::prelude::*;

/// How much of a file the preview plays. Long enough to recognise a jingle or a
/// time announcement, short enough to bound both the decode and the wait.
const PREVIEW_SECONDS: usize = 20;

/// Radio-box rows, in the order they are built. The index is the identity here —
/// never the label, which is translated.
const KIND_INTERVAL: i32 = 0;
const KIND_HOURLY: i32 = 1;
const KIND_DAILY: i32 = 2;

fn no_items() -> String {
    t!("No scheduled items")
}

/// One row of the item list: the file, when it plays, and whether it is on.
fn item_label(item: &ScheduleItem) -> String {
    let file = if item.file.trim().is_empty() {
        t!("No file")
    } else {
        crate::media::track_title(std::path::Path::new(item.file.trim()))
    };
    let when = item.trigger.describe();
    if item.enabled {
        t!("{file}: {when}", file = file, when = when)
    } else {
        // The whole phrase rather than a word appended to the line above:
        // languages agree the second half with the first.
        t!("{file}: {when} (turned off)", file = file, when = when)
    }
}

/// The Media Scheduler source dialog: the list of scheduled items.
pub fn edit_scheduler(
    app: &Rc<App>,
    scene_index: usize,
    source_index: usize,
    current: SchedulerSourceConfig,
) {
    let Some(frame) = app.widgets(|w| w.frame) else {
        return;
    };
    let dialog = Dialog::builder(&frame, &t!("Media Scheduler source"))
        .with_style(DialogStyle::DefaultDialogStyle | DialogStyle::ResizeBorder)
        .with_size(560, 460)
        .build();
    let panel = Panel::builder(&dialog).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    // `native_acc` takes the list's name from this label, so its text and the
    // name passed to `install` have to agree, and it has to stay in front of
    // the list.
    let list_label = StaticText::builder(&panel)
        .with_label(&t!("Scheduled items"))
        .build();
    let list = ListBox::builder(&panel).build();
    super::native_acc::install(&list, &t!("Scheduled items"));
    super::help::tag(&list, "dialog.schedulerSource.list", "Scheduled items list");

    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let add = Button::builder(&panel).with_label(&t!("Add")).build();
    super::help::tag(&add, "dialog.schedulerSource.add", "Add scheduled item");
    let edit = Button::builder(&panel).with_label(&t!("Edit")).build();
    super::help::tag(&edit, "dialog.schedulerSource.edit", "Edit scheduled item");
    // Worth its own button because the commonest schedule is one file at
    // several times: a twelve-hour chime is the same item twelve hours apart,
    // and duplicating it leaves only the time to change.
    let duplicate = Button::builder(&panel).with_label(&t!("Duplicate")).build();
    super::help::tag(
        &duplicate,
        "dialog.schedulerSource.duplicate",
        "Duplicate scheduled item",
    );
    let remove = Button::builder(&panel).with_label(&t!("Remove")).build();
    super::help::tag(
        &remove,
        "dialog.schedulerSource.remove",
        "Remove scheduled item",
    );
    buttons.add(&add, 0, SizerFlag::All, 4);
    buttons.add(&edit, 0, SizerFlag::All, 4);
    buttons.add(&duplicate, 0, SizerFlag::All, 4);
    buttons.add(&remove, 0, SizerFlag::All, 4);

    let note = StaticText::builder(&panel)
        .with_label(&t!(
            "Scheduled items play only while this source's scene is the active one, and a \
             file that was due while the scene was elsewhere is not played late. Put the \
             scheduler in every scene you broadcast from."
        ))
        .build();
    note.wrap(520);

    let confirm = BoxSizer::builder(Orientation::Horizontal).build();
    let ok = super::ok_button(&panel, &t!("OK"));
    // `ID_CANCEL` is what wx maps Escape to; without it Escape does nothing.
    let cancel = Button::builder(&panel)
        .with_id(ID_CANCEL)
        .with_label(&t!("Cancel"))
        .build();
    confirm.add(&ok, 0, SizerFlag::All, 4);
    confirm.add(&cancel, 0, SizerFlag::All, 4);

    sizer.add(&list_label, 0, SizerFlag::All, 4);
    sizer.add(&list, 1, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add_sizer(&buttons, 0, SizerFlag::Expand, 0);
    sizer.add(&note, 0, SizerFlag::Expand | SizerFlag::All, 8);
    sizer.add_sizer(&confirm, 0, SizerFlag::AlignRight, 0);
    panel.set_sizer(sizer, true);
    let dialog_sizer = BoxSizer::builder(Orientation::Vertical).build();
    dialog_sizer.add(&panel, 1, SizerFlag::Expand, 0);
    dialog.set_sizer(dialog_sizer, true);

    // The dialog's own working copy. Nothing reaches the config until OK, so
    // Cancel really cancels — including the removals.
    let items: Rc<RefCell<Vec<ScheduleItem>>> = Rc::new(RefCell::new(current.items));

    // `select` is a deliberate edit's landing place. A refresh that is not an
    // edit passes `None` and leaves the selection alone, because a screen reader
    // speaks a row as it becomes selected.
    let refresh: Rc<dyn Fn(Option<usize>)> = {
        let items = items.clone();
        Rc::new(move |select: Option<usize>| {
            let labels: Vec<String> = items.borrow().iter().map(item_label).collect();
            super::list::fill(&list, &labels, &no_items());
            if labels.is_empty() {
                return;
            }
            if let Some(index) = select {
                list.set_selection(index.min(labels.len() - 1) as u32, true);
            }
        })
    };
    refresh(None);

    let selected = {
        let items = items.clone();
        move || super::list::selection(&list, items.borrow().len())
    };

    {
        let items = items.clone();
        let refresh = refresh.clone();
        add.on_click(move |_| {
            let Some(item) = edit_item(&dialog, None) else {
                return;
            };
            let mut items = items.borrow_mut();
            items.push(item);
            let last = items.len() - 1;
            drop(items);
            refresh(Some(last));
        });
    }
    {
        let items = items.clone();
        let refresh = refresh.clone();
        let selected = selected.clone();
        edit.on_click(move |_| {
            let Some(index) = selected() else {
                return;
            };
            // Copied out before the nested dialog runs: it re-enters the pump,
            // and nothing may hold this borrow across that.
            let existing = items.borrow().get(index).cloned();
            let Some(existing) = existing else {
                return;
            };
            let Some(edited) = edit_item(&dialog, Some(&existing)) else {
                return;
            };
            if let Some(slot) = items.borrow_mut().get_mut(index) {
                *slot = edited;
            }
            refresh(Some(index));
        });
    }
    {
        let items = items.clone();
        let refresh = refresh.clone();
        let selected = selected.clone();
        duplicate.on_click(move |_| {
            let Some(index) = selected() else {
                return;
            };
            let mut items = items.borrow_mut();
            let Some(copy) = items.get(index).cloned() else {
                return;
            };
            // Straight after the original rather than at the end: the two are
            // the same file at different times, and a list read top to bottom
            // should keep them together.
            items.insert(index + 1, copy);
            drop(items);
            refresh(Some(index + 1));
        });
    }
    {
        let items = items.clone();
        let refresh = refresh.clone();
        let selected = selected.clone();
        remove.on_click(move |_| {
            let Some(index) = selected() else {
                return;
            };
            let mut items = items.borrow_mut();
            if index >= items.len() {
                return;
            }
            items.remove(index);
            let landing = index.min(items.len().saturating_sub(1));
            let empty = items.is_empty();
            drop(items);
            refresh((!empty).then_some(landing));
        });
    }
    {
        ok.on_click(move |_| dialog.end_modal(ID_OK));
    }
    {
        cancel.on_click(move |_| dialog.end_modal(ID_CANCEL));
    }

    let outcome = dialog.show_modal();
    if outcome == ID_OK {
        let mut settings = SchedulerSourceConfig {
            items: items.borrow().clone(),
        };
        settings.fix_up();
        super::scenes::set_source_kind(
            app,
            scene_index,
            source_index,
            SourceKindConfig::Scheduler(settings),
        );
    }
    dialog.destroy();
}

/// The editor for one scheduled item. `existing` is `None` for Add.
fn edit_item(parent: &Dialog, existing: Option<&ScheduleItem>) -> Option<ScheduleItem> {
    let adding = existing.is_none();
    let caption = if adding {
        t!("Add scheduled item")
    } else {
        t!("Edit scheduled item")
    };
    let dialog = Dialog::builder(parent, &caption)
        .with_style(DialogStyle::DefaultDialogStyle)
        .with_size(520, 400)
        .build();
    let panel = Panel::builder(&dialog).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let file_label = StaticText::builder(&panel).with_label(&t!("File")).build();
    let file_input = TextCtrl::builder(&panel)
        .with_value(existing.map(|i| i.file.as_str()).unwrap_or(""))
        .build();
    super::set_accessible_name(&file_input, &t!("File"));
    super::help::tag(
        &file_input,
        "dialog.scheduleItem.file",
        "Scheduled file path box",
    );
    let browse = Button::builder(&panel).with_label(&t!("Browse...")).build();
    super::help::tag(
        &browse,
        "dialog.scheduleItem.browse",
        "Browse for a scheduled file",
    );
    let preview = Button::builder(&panel).with_label(&t!("Preview")).build();
    super::set_accessible_name(&preview, &t!("Preview"));
    super::help::tag(
        &preview,
        "dialog.scheduleItem.preview",
        "Preview the scheduled file",
    );

    let enabled = CheckBox::builder(&panel)
        .with_label(&t!("Play this item"))
        .build();
    super::set_accessible_name(&enabled, &t!("Play this item"));
    super::help::tag(
        &enabled,
        "dialog.scheduleItem.enabled",
        "Whether the item plays",
    );
    enabled.set_value(existing.map(|i| i.enabled).unwrap_or(true));

    let kind_labels = [
        t!("Every so many minutes"),
        t!("Every hour"),
        t!("Every day"),
    ];
    let kind_labels: Vec<&str> = kind_labels.iter().map(String::as_str).collect();
    let kind = RadioBox::builder(&panel, &kind_labels)
        .with_label(&t!("When it plays"))
        .with_style(RadioBoxStyle::SpecifyRows)
        .with_major_dimension(1)
        .build();
    // A radio box's items are real child windows, so `set_accessible_name` can
    // only reach the group. `install_radio_box` hands the whole group back to
    // OLEACC, which names each button from its own text.
    super::native_acc::install_radio_box(&kind, &t!("When it plays"));
    super::help::tag(&kind, "dialog.scheduleItem.kind", "When the item plays");

    // --- Every so many minutes ---
    let interval_label = StaticText::builder(&panel)
        .with_label(&t!("Interval"))
        .build();
    let interval = Choice::builder(&panel).build();
    for minutes in INTERVALS {
        interval.append(&interval_row(*minutes));
    }
    super::set_accessible_name(&interval, &t!("Interval"));
    super::help::tag(
        &interval,
        "dialog.scheduleItem.interval",
        "How often the item plays",
    );

    // --- Every hour ---
    let past_label = StaticText::builder(&panel)
        .with_label(&t!("Minutes past the hour"))
        .build();
    let past = SpinCtrl::builder(&panel).with_range(0, 59).build();
    super::set_accessible_name(&past, &t!("Minutes past the hour"));
    super::help::tag(
        &past,
        "dialog.scheduleItem.minutesPast",
        "Minutes past the hour",
    );

    // --- Every day ---
    let time_label = StaticText::builder(&panel).with_label(&t!("Time")).build();
    let time = TimePickerCtrl::builder(&panel).build();
    super::set_accessible_name(&time, &t!("Time"));
    // A native `SysDateTimePick32` announces nothing as the caret moves between
    // its fields; this is what speaks them. See `ui/picker_acc.rs`.
    super::picker_acc::install(&time, super::picker_acc::Kind::Time);
    super::help::tag(&time, "dialog.scheduleItem.time", "Time of day to play at");

    // Every control starts from the item being edited, whichever kind it is, so
    // switching between the three and back does not lose what was typed.
    let (selected_kind, interval_index, past_value, time_value) =
        controls_from(existing.map(|i| i.trigger).unwrap_or_default());
    kind.set_selection(selected_kind);
    interval.set_selection(interval_index as u32);
    past.set_value(past_value as i32);
    time.set_value(&DateTime::new(
        2000,
        1,
        1,
        (time_value / 60) as i16,
        (time_value % 60) as i16,
        0,
    ));

    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let ok = super::ok_button(&panel, &t!("OK"));
    let cancel = Button::builder(&panel)
        .with_id(ID_CANCEL)
        .with_label(&t!("Cancel"))
        .build();
    buttons.add(&ok, 0, SizerFlag::All, 4);
    buttons.add(&cancel, 0, SizerFlag::All, 4);

    let file_row = BoxSizer::builder(Orientation::Horizontal).build();
    file_row.add(&file_input, 1, SizerFlag::Expand | SizerFlag::All, 4);
    file_row.add(&browse, 0, SizerFlag::All, 4);
    file_row.add(&preview, 0, SizerFlag::All, 4);
    sizer.add(&file_label, 0, SizerFlag::All, 4);
    sizer.add_sizer(&file_row, 0, SizerFlag::Expand, 0);
    sizer.add(&enabled, 0, SizerFlag::All, 4);
    sizer.add(&kind, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&interval_label, 0, SizerFlag::All, 4);
    sizer.add(&interval, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&past_label, 0, SizerFlag::All, 4);
    sizer.add(&past, 0, SizerFlag::All, 4);
    sizer.add(&time_label, 0, SizerFlag::All, 4);
    sizer.add(&time, 0, SizerFlag::All, 4);
    sizer.add_sizer(&buttons, 0, SizerFlag::AlignRight, 0);
    panel.set_sizer(sizer, true);
    let dialog_sizer = BoxSizer::builder(Orientation::Vertical).build();
    dialog_sizer.add(&panel, 1, SizerFlag::Expand, 0);
    dialog.set_sizer(dialog_sizer, true);

    // Show/hide rather than enable/disable — see the module header.
    let apply_kind = move || {
        let selection = kind.get_selection();
        interval_label.show(selection == KIND_INTERVAL);
        interval.show(selection == KIND_INTERVAL);
        past_label.show(selection == KIND_HOURLY);
        past.show(selection == KIND_HOURLY);
        time_label.show(selection == KIND_DAILY);
        time.show(selection == KIND_DAILY);
        panel.layout();
    };
    apply_kind();
    {
        kind.clone().on_selected(move |_| apply_kind());
    }

    {
        browse.on_click(move |_| {
            // Every filter is built from the decoder's own list, so the dialog
            // cannot offer a file the scheduler would then fail to play.
            let patterns = crate::media::SUPPORTED_EXTENSIONS
                .iter()
                .map(|e| format!("*.{e}"))
                .collect::<Vec<_>>()
                .join(";");
            let picker = FileDialog::builder(&dialog)
                .with_message(&t!("Choose a file to schedule"))
                .with_default_dir(&parent_folder(&file_input.get_value()))
                .with_wildcard(&t!(
                    "Audio files ({patterns})|{patterns}|All files (*.*)|*.*",
                    patterns = patterns
                ))
                .with_style(FileDialogStyle::Open | FileDialogStyle::FileMustExist)
                .build();
            let chosen = if picker.show_modal() == ID_OK {
                picker.get_path()
            } else {
                None
            };
            if let Some(path) = chosen {
                file_input.set_value(&path);
            }
            file_input.set_focus();
        });
    }

    // The cue this dialog started, if one is still playing. It outlives the
    // click — `play_samples_handle` returns at once and the sound goes on — so
    // it is held here and stopped when the dialog closes, rather than left to
    // play on into a window that has gone.
    //
    // Nothing may hold a borrow of this across a call that shows a window: the
    // message boxes below re-enter the pump, which can run this same callback.
    let playing: Rc<RefCell<Option<crate::audio::cue::CuePlayback>>> = Rc::new(RefCell::new(None));
    {
        let playing = playing.clone();
        preview.on_click(move |_| {
            // A second press stops the first, so a long file is not something
            // to sit through. Taken before anything that shows a window.
            if let Some(handle) = playing.borrow_mut().take() {
                handle.stop();
                return;
            }
            let path = file_input.get_value().trim().to_string();
            if path.is_empty() {
                super::show_warning(
                    &dialog,
                    &t!("Preview"),
                    &t!("Choose a file first."),
                );
                file_input.set_focus();
                return;
            }
            match preview_samples(std::path::Path::new(&path)) {
                Ok(samples) => {
                    *playing.borrow_mut() =
                        Some(crate::audio::cue::play_samples_handle(std::sync::Arc::new(samples)));
                }
                Err(message) => {
                    super::show_error(
                        &dialog,
                        &t!("Preview"),
                        &t!(
                            "Pubsplash could not play that file: {message}",
                            message = message
                        ),
                    );
                    file_input.set_focus();
                }
            }
        });
    }

    let result: Rc<RefCell<Option<ScheduleItem>>> = Rc::new(RefCell::new(None));
    {
        let result = result.clone();
        ok.on_click(move |_| {
            let file = file_input.get_value().trim().to_string();
            if file.is_empty() {
                // No `end_modal`: the dialog stays open behind the warning with
                // the rest of the item intact.
                super::show_warning(
                    &dialog,
                    &caption,
                    &t!("Choose a file for this item to play."),
                );
                file_input.set_focus();
                return;
            }
            let clock = time.get_value();
            let mut trigger = match kind.get_selection() {
                KIND_INTERVAL => ScheduleTrigger::EveryMinutes {
                    minutes: interval
                        .get_selection()
                        .and_then(|i| INTERVALS.get(i as usize).copied())
                        .unwrap_or(60),
                },
                KIND_DAILY => ScheduleTrigger::DailyAt {
                    hour: clock.hour().max(0) as u32,
                    minute: clock.minute().max(0) as u32,
                },
                // `KIND_HOURLY`, and anything a future row might add: hourly is
                // the default a trigger falls back to everywhere else too.
                _ => ScheduleTrigger::Hourly {
                    minute: past.value().max(0) as u32,
                },
            };
            trigger.fix_up();
            *result.borrow_mut() = Some(ScheduleItem {
                file,
                enabled: enabled.get_value(),
                trigger,
            });
            dialog.end_modal(ID_OK);
        });
    }
    {
        cancel.on_click(move |_| dialog.end_modal(ID_CANCEL));
    }

    let outcome = dialog.show_modal();
    // A preview still going would otherwise play on into a dialog that has gone.
    if let Some(handle) = playing.borrow_mut().take() {
        handle.stop();
    }
    let item = if outcome == ID_OK {
        result.borrow_mut().take()
    } else {
        None
    };
    dialog.destroy();
    item
}

/// The three trigger controls' values for a given trigger.
///
/// Every one of them is filled, whichever kind the trigger is, so that switching
/// the radio box to another kind and back does not lose what was there. The two
/// that do not apply fall back to something sensible rather than zero.
fn controls_from(trigger: ScheduleTrigger) -> (i32, usize, u32, u32) {
    let default_interval = INTERVALS.iter().position(|m| *m == 60).unwrap_or(0);
    match trigger {
        ScheduleTrigger::EveryMinutes { minutes } => (
            KIND_INTERVAL,
            // A hand-edited interval that is not on the list falls back to the
            // nearest one at or below it, so the dialog never silently speeds a
            // schedule up.
            INTERVALS
                .iter()
                .rposition(|m| *m <= minutes)
                .unwrap_or(default_interval),
            0,
            0,
        ),
        ScheduleTrigger::Hourly { minute } => (
            KIND_HOURLY,
            default_interval,
            schedule::clamp_minute(minute),
            schedule::clamp_minute(minute),
        ),
        ScheduleTrigger::DailyAt { hour, minute } => (
            KIND_DAILY,
            default_interval,
            schedule::clamp_minute(minute),
            schedule::clamp_hour(hour) * 60 + schedule::clamp_minute(minute),
        ),
    }
}

/// One row of the interval dropdown, worded the way the item's own row is.
fn interval_row(minutes: u32) -> String {
    ScheduleTrigger::EveryMinutes { minutes }.describe()
}

/// The folder a path is in, for the file picker to open in. Empty when there is
/// none, which wx reads as "wherever you were last".
fn parent_folder(path: &str) -> String {
    std::path::Path::new(path.trim())
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The first [`PREVIEW_SECONDS`] of a file, decoded exactly as the scheduler
/// will decode it.
fn preview_samples(path: &std::path::Path) -> Result<Vec<f32>, String> {
    use std::ops::ControlFlow;
    let limit = PREVIEW_SECONDS
        * crate::audio::mixer::SAMPLE_RATE as usize
        * crate::audio::mixer::CHANNELS;
    let mut samples: Vec<f32> = Vec::new();
    crate::media::decode::stream_file(path, &mut |chunk: &[f32]| {
        samples.extend_from_slice(chunk);
        if samples.len() >= limit {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })?;
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(file: &str, enabled: bool, trigger: ScheduleTrigger) -> ScheduleItem {
        ScheduleItem {
            file: file.to_string(),
            enabled,
            trigger,
        }
    }

    /// A row has to say all three things a user needs to check at a glance:
    /// which file, when, and whether it is live.
    #[test]
    fn a_row_names_the_file_the_time_and_whether_it_is_on() {
        let row = item_label(&item(
            r"O:\radio\09.mp3",
            true,
            ScheduleTrigger::DailyAt { hour: 9, minute: 0 },
        ));
        assert!(row.contains("09"), "{row}");
        assert!(row.contains("09:00"), "{row}");

        let off = item_label(&item(
            r"O:\radio\09.mp3",
            false,
            ScheduleTrigger::DailyAt { hour: 9, minute: 0 },
        ));
        assert_ne!(off, row, "a disabled item must read differently");
    }

    #[test]
    fn an_item_with_no_file_still_has_a_readable_row() {
        let row = item_label(&item("   ", true, ScheduleTrigger::Hourly { minute: 0 }));
        assert!(!row.trim().is_empty());
    }

    /// Switching the radio box between kinds must not lose what the other two
    /// controls were showing, and must not zero a time the user set.
    #[test]
    fn every_control_is_filled_whichever_kind_the_trigger_is() {
        let (kind, interval, past, time) =
            controls_from(ScheduleTrigger::DailyAt { hour: 13, minute: 45 });
        assert_eq!(kind, KIND_DAILY);
        assert_eq!(time, 13 * 60 + 45);
        assert_eq!(past, 45, "the hourly box keeps the minute");
        assert!(INTERVALS.get(interval).is_some());

        let (kind, interval, ..) = controls_from(ScheduleTrigger::EveryMinutes { minutes: 15 });
        assert_eq!(kind, KIND_INTERVAL);
        assert_eq!(INTERVALS[interval], 15);
    }

    /// A settings file naming an interval the dropdown does not offer must land
    /// on one at or below it — never above, which would make the item play more
    /// often than it was asked to.
    #[test]
    fn an_off_list_interval_rounds_down() {
        let (_, interval, ..) = controls_from(ScheduleTrigger::EveryMinutes { minutes: 17 });
        assert_eq!(INTERVALS[interval], 15);
        let (_, interval, ..) = controls_from(ScheduleTrigger::EveryMinutes { minutes: 1 });
        assert_eq!(INTERVALS[interval], 1);
    }

    #[test]
    fn a_file_picker_opens_in_the_folder_the_current_file_is_in() {
        assert_eq!(parent_folder(r"O:\radio\hours\09.mp3"), r"O:\radio\hours");
        assert_eq!(parent_folder("   "), "");
    }
}
