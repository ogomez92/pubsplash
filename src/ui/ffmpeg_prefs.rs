//! The FFmpeg group in Preferences > Audio, and the download that fills it in.
//!
//! FFmpeg is needed by exactly one thing — an RTMP service, which YouTube is —
//! and by nothing else in the app. It lives on the Audio tab rather than a tab
//! of its own because the notebook is already nine pages wide, and one more
//! grows the pair of scroll arrows that hide tabs off the end (see the wxDragon
//! notes in CLAUDE.md).
//!
//! # The progress dialog
//!
//! Modelled on [`super::scan_dialog`], and for the reasons its header sets out
//! at length: a `wxProgressDialog` on MSW is a native task dialog on its own
//! thread whose `Update` re-enters the pump, and whose constructor can hang
//! outright. So this is a plain modeless `Dialog` that does nothing on its own —
//! the pump writes to it, and Cancel sets the worker's atomic from its own click
//! handler. No nested event loop is involved anywhere.
//!
//! A native `Gauge` is what makes it accessible: it reports its own percentage
//! to a screen reader, so a download that takes several minutes can be checked
//! on by tabbing to it rather than by watching. The status text beside it is a
//! read-only `TextCtrl` for the same reason it is in the scan dialog — writing
//! to one announces nothing by itself, so progress cannot talk over the user.
//!
//! # Closing Preferences cancels
//!
//! The dialog's parent is the Preferences dialog, so it must not outlive it;
//! `preferences::show` takes and drops the state on its way out, and dropping it
//! raises the cancel flag. The same rule the plugin scan follows, and the status
//! text says so rather than leaving it to be discovered.

use crate::t;
use super::{App, show_error, show_info};
use crate::ffmpeg::{self, install};
use std::rc::Rc;
use wxdragon::prelude::*;

/// The live download: the worker's channel, its cancel flag, and the window.
pub struct Download {
    pub events: crossbeam_channel::Receiver<install::Progress>,
    pub cancel: install::Cancel,
    pub dialog: Rc<ProgressDialog>,
    /// The Preferences status line to refresh when this finishes. Held as a
    /// widget handle, which is sound only because this whole struct is dropped
    /// when Preferences closes.
    pub status: TextCtrl,
    /// Re-enabled when the download ends, so a second one cannot be started on
    /// top of the first.
    pub button: Button,
}

impl Drop for Download {
    /// Raises the cancel flag so a Preferences close stops the worker.
    ///
    /// The worker is *not* joined: it is parked in a body read that can take
    /// seconds to notice, and blocking the UI thread on it would freeze the app
    /// while a dialog was closing. It checks the flag once per chunk and cleans
    /// up its own partial file, so letting it finish on its own is safe.
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The modeless progress window. Dropping it destroys the window.
pub struct ProgressDialog {
    dialog: Dialog,
    gauge: Gauge,
    status: TextCtrl,
}

impl Drop for ProgressDialog {
    fn drop(&mut self) {
        self.dialog.destroy();
    }
}

impl ProgressDialog {
    fn show(parent: &Dialog, cancel: install::Cancel) -> Self {
        let dialog = Dialog::builder(parent, "Downloading FFmpeg")
            .with_style(DialogStyle::DefaultDialogStyle)
            .with_size(480, 220)
            .build();
        let panel = Panel::builder(&dialog).build();
        let sizer = BoxSizer::builder(Orientation::Vertical).build();

        let gauge_label = StaticText::builder(&panel).with_label(&t!("Progress")).build();
        let gauge = Gauge::builder(&panel).with_range(100).build();
        super::set_accessible_name(&gauge, &t!("Download progress"));
        super::help::tag(&gauge, "dialog.ffmpeg.progress", "FFmpeg download progress bar");

        let status_label = StaticText::builder(&panel).with_label(&t!("Status")).build();
        let status = TextCtrl::builder(&panel)
            .with_style(TextCtrlStyle::ReadOnly)
            .with_value(&format!(
                "Starting. This is about {} MB, and closing Preferences will cancel it.",
                install::APPROXIMATE_MEGABYTES
            ))
            .build();
        super::set_accessible_name(&status, &t!("Download status"));
        super::help::tag(&status, "dialog.ffmpeg.status", "FFmpeg download status text");

        // Dismiss-only in shape, so `dismiss_button` puts both Escape and Enter
        // on it; the click handler holds the flag itself, so cancelling needs no
        // borrow of anything and cannot be tripped up by whatever the pump is
        // doing.
        let cancel_button = super::dismiss_button(&panel, &t!("Cancel download"));
        {
            let cancel = cancel.clone();
            cancel_button.on_click(move |_| {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            });
        }

        sizer.add(&gauge_label, 0, SizerFlag::All, 4);
        sizer.add(&gauge, 0, SizerFlag::Expand | SizerFlag::All, 4);
        sizer.add(&status_label, 0, SizerFlag::All, 4);
        sizer.add(&status, 0, SizerFlag::Expand | SizerFlag::All, 4);
        sizer.add(&cancel_button, 0, SizerFlag::All, 8);
        panel.set_sizer(sizer, true);
        let dialog_sizer = BoxSizer::builder(Orientation::Vertical).build();
        dialog_sizer.add(&panel, 1, SizerFlag::Expand, 0);
        dialog.set_sizer(dialog_sizer, true);
        dialog.show(true);
        Self {
            dialog,
            gauge,
            status,
        }
    }

    fn set(&self, percent: i32, message: &str) {
        self.gauge.set_value(percent);
        self.status.set_value(message);
    }
}

/// What the Preferences status line says about the ffmpeg currently in use.
///
/// Names the *path*, not just "found": the whole point of the setting below is
/// that there may be more than one on the machine, and a user debugging a
/// missing encoder needs to know which one answered.
fn describe(app: &App) -> String {
    let configured = app.config.borrow().connection.ffmpeg_path.clone();
    match ffmpeg::locate(&configured) {
        Ok(path) => match ffmpeg::probe(&path) {
            Ok(caps) => format!(
                "Using {} — {}, H.264 via {}, AAC via {}.",
                path.display(),
                caps.version,
                caps.h264,
                caps.aac
            ),
            Err(problem) => format!("Found {} but it cannot be used: {problem}", path.display()),
        },
        Err(problem) => problem,
    }
}

/// Builds the group box. Returns it for the caller to add to its sizer.
pub fn build_group(app: &Rc<App>, dialog: &Dialog, panel: &Panel) -> StaticBoxSizer {
    let (group, group_box) = super::group_box(panel, &t!("Streaming to YouTube (FFmpeg)"));

    // Read-only rather than a StaticText, so it is a Tab stop a screen-reader
    // user can land on and read on demand. A StaticText is skipped by Tab and
    // announced only in passing, and this is the one place that says *which*
    // ffmpeg is in use.
    // One binding, read twice: the visible label and the accessible name of the
    // box beside it have to stay the same words.
    let status_text = t!("FFmpeg status");
    let status_label = StaticText::builder(&group_box)
        .with_label(&status_text)
        .build();
    let status = TextCtrl::builder(&group_box)
        .with_style(TextCtrlStyle::ReadOnly)
        .build();
    super::set_accessible_name(&status, &status_text);
    super::help::tag(
        &status,
        "dialog.preferences.audio.ffmpegStatus",
        "FFmpeg status text",
    );

    let location_text = t!("FFmpeg location (leave blank to search automatically)");
    let location_label = StaticText::builder(&group_box)
        .with_label(&location_text)
        .build();
    let location = TextCtrl::builder(&group_box)
        .with_value(&app.config.borrow().connection.ffmpeg_path)
        .build();
    super::set_accessible_name(&location, &location_text);
    super::help::tag(
        &location,
        "dialog.preferences.audio.ffmpegPath",
        "FFmpeg location box",
    );

    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let browse = Button::builder(&group_box)
        .with_label(&t!("Choose FFmpeg"))
        .build();
    super::help::tag(
        &browse,
        "dialog.preferences.audio.ffmpegBrowse",
        "Choose FFmpeg button",
    );
    let download = Button::builder(&group_box)
        .with_label(&t!("Download FFmpeg"))
        .build();
    super::help::tag(
        &download,
        "dialog.preferences.audio.ffmpegDownload",
        "Download FFmpeg button",
    );
    buttons.add(&browse, 0, SizerFlag::All, 4);
    buttons.add(&download, 0, SizerFlag::All, 4);

    group.add(&status_label, 0, SizerFlag::All, 4);
    group.add(&status, 0, SizerFlag::Expand | SizerFlag::All, 4);
    group.add(&location_label, 0, SizerFlag::All, 4);
    group.add(&location, 0, SizerFlag::Expand | SizerFlag::All, 4);
    group.add_sizer(&buttons, 0, SizerFlag::Expand, 0);

    // Probing runs two processes, so this is deliberately not done on every
    // keystroke: the box saves as it is typed (like every other Preferences
    // control), and the status line is re-read only when the user has finished
    // — on focus leaving the box, and after Choose or Download.
    let refresh = {
        let app = app.clone();
        move || status.set_value(&describe(&app))
    };
    refresh();

    {
        let app = app.clone();
        let refresh = refresh.clone();
        location.clone().on_kill_focus(move |event| {
            app.config.borrow_mut().connection.ffmpeg_path =
                location.get_value().trim().to_string();
            app.save_config();
            refresh();
            event.skip(true);
        });
    }

    {
        let app = app.clone();
        let refresh = refresh.clone();
        let dialog = *dialog;
        browse.on_click(move |_| {
            let picker = FileDialog::builder(&dialog)
                .with_message("Choose ffmpeg.exe")
                .with_wildcard("FFmpeg (ffmpeg.exe)|ffmpeg.exe|Programs (*.exe)|*.exe")
                .with_style(FileDialogStyle::Open | FileDialogStyle::FileMustExist)
                .build();
            if picker.show_modal() != ID_OK {
                return;
            }
            let Some(path) = picker.get_path() else { return };
            location.set_value(&path);
            app.config.borrow_mut().connection.ffmpeg_path = path.trim().to_string();
            app.save_config();
            refresh();
        });
    }

    {
        let app = app.clone();
        let dialog = *dialog;
        download.on_click(move |_| begin_download(&app, &dialog, status, download));
    }

    group
}

/// Starts the download, or explains why it will not.
fn begin_download(app: &Rc<App>, dialog: &Dialog, status: TextCtrl, button: Button) {
    // Recorded before the window exists, for the reason `preferences::begin_scan`
    // spells out: showing a window dispatches messages, so a second click can
    // land while the first is still inside the constructor — and with the
    // assignment last, the guard would wave every one of them through and start
    // a download thread each time.
    if app.ffmpeg_download.borrow().is_some() {
        return;
    }
    if !app
        .config
        .borrow()
        .connection
        .ffmpeg_path
        .trim()
        .is_empty()
    {
        show_error(
            dialog,
            &t!("Download FFmpeg"),
            &t!("Clear the FFmpeg location box first. While it names a file, that file is the \
             one Pubsplash uses, so a downloaded copy would be ignored."),
        );
        return;
    }
    let confirm = MessageDialog::builder(
        dialog,
        &format!(
            "Pubsplash will download FFmpeg from gyan.dev, which is the Windows build \
             ffmpeg.org itself points at. It is about {} MB and is checked against the \
             checksum published beside it.\n\nDownload it now?",
            install::APPROXIMATE_MEGABYTES
        ),
        "Download FFmpeg",
    )
    .with_style(MessageDialogStyle::YesNo | MessageDialogStyle::IconQuestion)
    .build();
    let answer = confirm.show_modal();
    confirm.destroy();
    if answer != ID_YES {
        return;
    }

    button.enable(false);
    let (events, receiver) = crossbeam_channel::unbounded();
    let cancel = install::cancel_flag();
    let progress = Rc::new(ProgressDialog::show(dialog, cancel.clone()));
    *app.ffmpeg_download.borrow_mut() = Some(Download {
        events: receiver,
        cancel: cancel.clone(),
        dialog: progress,
        status,
        button,
    });
    install::start(events, cancel);
    // Bring the fast timer up now rather than waiting for the next idle to
    // notice the download exists.
    super::sync_fast_timer(app);
}

/// Drains the worker's progress onto the dialog. Called from the pump.
///
/// Nothing here holds an `app.ffmpeg_download` borrow across a call into wx: the
/// finishing paths raise a message box, which dispatches messages and can
/// re-enter the pump.
pub fn drain(app: &Rc<App>) {
    let Some((events, dialog)) = app
        .ffmpeg_download
        .borrow()
        .as_ref()
        .map(|download| (download.events.clone(), download.dialog.clone()))
    else {
        return;
    };

    let mut finished: Option<install::Progress> = None;
    while let Ok(progress) = events.try_recv() {
        match progress {
            install::Progress::Downloading { done, total } => {
                // `total` is zero when the server sent no `Content-Length`, in
                // which case there is no percentage to show and the bar stays
                // where it is rather than jumping to a made-up number.
                let percent = done
                    .saturating_mul(100)
                    .checked_div(total)
                    .map_or(0, |percent| percent.min(100) as i32);
                dialog.set(
                    percent,
                    &format!(
                        "Downloaded {} MB of {} MB.",
                        done / 1_000_000,
                        total / 1_000_000
                    ),
                );
            }
            install::Progress::Extracting => {
                dialog.set(100, "Downloaded and checked. Unpacking FFmpeg.");
            }
            // The three terminal ones end the loop rather than being handled
            // here, so the window and the state are gone before any message box
            // goes up on top of them.
            terminal => {
                finished = Some(terminal);
                break;
            }
        }
    }

    let Some(outcome) = finished else { return };
    drop(dialog);
    let state = app.ffmpeg_download.borrow_mut().take();
    let Some(state) = state else { return };
    let (button, status) = (state.button, state.status);
    // Explicitly, and before the message box below: `state` owns the last
    // reference to the progress window, and letting it fall out of scope at the
    // end of this function would leave that window standing behind the box that
    // is announcing the download has finished.
    drop(state);
    // The fast timer was only up for this download. Nothing else calls this on
    // the way out, so without it the tick runs on doing nothing until the next
    // editor or scan happens to bring it down.
    super::sync_fast_timer(app);
    // Re-read before the message box: the status line is behind the box, and a
    // user dismissing it should find it already correct.
    button.enable(true);
    status.set_value(&describe(app));

    let frame = app.widgets(|w| w.frame);
    let Some(frame) = frame else { return };
    match outcome {
        install::Progress::Installed { path, version } => show_info(
            &frame,
            &t!("Download FFmpeg"),
            &format!("FFmpeg is installed and working.\n\n{version}\n{}", path.display()),
        ),
        install::Progress::Failed { message } => {
            show_error(&frame, &t!("Download FFmpeg"), &message);
        }
        install::Progress::Cancelled => {
            log::info!("FFmpeg download cancelled");
        }
        // Unreachable: only the three above break the loop.
        install::Progress::Downloading { .. } | install::Progress::Extracting => {}
    }
}
