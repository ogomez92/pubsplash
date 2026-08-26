//! Setup streaming services dialog: service list, credentials, connect/disconnect.

use super::{App, show_error};
use crate::config::{MAIN_SITE_URL, SiteConfig, StreamingServiceType};
use crate::net::NetCommand;
use crate::secret::Secret;
use std::rc::Rc;
use wxdragon::prelude::*;

/// Shown when no services are configured. See [`super::list`].
const NO_SERVICES: &str = "No services";

/// The service-type radio box's rows, in order.
///
/// One table, because the index is read and written in five places — building
/// the box, the field-visibility switch, loading a service, saving one, and the
/// Add dialog — and a bare `== 1` in each of them is what makes adding a third
/// type a hunt rather than an edit.
const SERVICE_TYPES: [(&str, StreamingServiceType); 3] = [
    ("Audiopub", StreamingServiceType::Audiopub),
    ("Icecast", StreamingServiceType::Icecast),
    ("YouTube", StreamingServiceType::Youtube),
];

fn service_type_labels() -> Vec<&'static str> {
    SERVICE_TYPES.iter().map(|(label, _)| *label).collect()
}

/// The type a selected row means. An out-of-range index falls back to the first
/// row rather than panicking: `get_selection` answers `-1` for a box with no
/// selection, which is what a freshly built one has.
fn service_type_at(index: i32) -> StreamingServiceType {
    usize::try_from(index)
        .ok()
        .and_then(|index| SERVICE_TYPES.get(index))
        .map(|(_, kind)| *kind)
        .unwrap_or(StreamingServiceType::Audiopub)
}

fn service_type_index(kind: StreamingServiceType) -> i32 {
    SERVICE_TYPES
        .iter()
        .position(|(_, candidate)| *candidate == kind)
        .unwrap_or(0) as i32
}

/// How a service's kind is written in the services list.
fn service_type_name(kind: StreamingServiceType) -> &'static str {
    SERVICE_TYPES
        .iter()
        .find(|(_, candidate)| *candidate == kind)
        .map(|(label, _)| *label)
        .unwrap_or("Audiopub")
}

pub fn show(app: &Rc<App>, frame: &Frame) {
    let dialog = Dialog::builder(frame, "Setup streaming services")
        .with_style(DialogStyle::DefaultDialogStyle | DialogStyle::ResizeBorder)
        // Grown by one label-and-field row when the listener count URL was
        // added: the services list is the only thing here with a proportion, so
        // anything else that appears comes straight out of it.
        .with_size(560, 700)
        .build();
    let panel = Panel::builder(&dialog).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();

    let services_label = StaticText::builder(&panel)
        .with_label("Streaming services")
        .build();
    let services_list = ListBox::builder(&panel).build();
    super::native_acc::install(&services_list, "Streaming services");
    super::help::tag(
        &services_list,
        "dialog.connect.siteList",
        "Configured streaming services list",
    );
    let service_buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let add_service = Button::builder(&panel).with_label("Add service").build();
    let rename_service = Button::builder(&panel)
        .with_label("Rename service")
        .build();
    let remove_service = Button::builder(&panel)
        .with_label("Remove service")
        .build();
    super::help::tag(&add_service, "dialog.connect.addSite", "Add service button");
    super::help::tag(
        &rename_service,
        "dialog.connect.renameSite",
        "Rename service button",
    );
    super::help::tag(
        &remove_service,
        "dialog.connect.removeSite",
        "Remove service button",
    );
    service_buttons.add(&add_service, 0, SizerFlag::All, 4);
    service_buttons.add(&rename_service, 0, SizerFlag::All, 4);
    service_buttons.add(&remove_service, 0, SizerFlag::All, 4);

    let service_type = RadioBox::builder(&panel, &service_type_labels())
        .with_label("Service type")
        .with_style(RadioBoxStyle::SpecifyRows)
        .with_major_dimension(1)
        .build();
    super::native_acc::install_radio_box(&service_type, "Service type");
    super::help::tag(
        &service_type,
        "dialog.connect.serviceType",
        "Service type selector",
    );

    let url_label = StaticText::builder(&panel)
        .with_label("Audiopub URL")
        .build();
    let url_input = TextCtrl::builder(&panel).build();
    super::set_accessible_name(&url_input, "Audiopub URL");
    super::help::tag(
        &url_input,
        "dialog.connect.url",
        "Audiopub URL for the selected service",
    );
    let email_label = StaticText::builder(&panel).with_label("Email").build();
    let email_input = TextCtrl::builder(&panel).build();
    super::set_accessible_name(&email_input, "Email");
    super::help::tag(
        &email_input,
        "dialog.connect.email",
        "Email address for the selected service",
    );
    let password_label = StaticText::builder(&panel).with_label("Password").build();
    let password_input = TextCtrl::builder(&panel)
        .with_style(TextCtrlStyle::Password)
        .build();
    super::set_accessible_name(&password_input, "Password");
    super::help::tag(
        &password_input,
        "dialog.connect.password",
        "Password for the selected service",
    );

    let server_label = StaticText::builder(&panel)
        .with_label("Icecast server")
        .build();
    let server_input = TextCtrl::builder(&panel).build();
    super::set_accessible_name(&server_input, "Icecast server");
    super::help::tag(
        &server_input,
        "dialog.connect.icecastServer",
        "Icecast server for the selected service",
    );
    let port_label = StaticText::builder(&panel)
        .with_label("Icecast port")
        .build();
    let port_input = TextCtrl::builder(&panel).build();
    super::set_accessible_name(&port_input, "Icecast port");
    super::help::tag(
        &port_input,
        "dialog.connect.icecastPort",
        "Icecast port for the selected service",
    );
    let mount_label = StaticText::builder(&panel)
        .with_label("Icecast mount point")
        .build();
    let mount_input = TextCtrl::builder(&panel).build();
    super::set_accessible_name(&mount_input, "Icecast mount point");
    super::help::tag(
        &mount_input,
        "dialog.connect.icecastMount",
        "Icecast mount point for the selected service",
    );
    let listeners_label = StaticText::builder(&panel)
        .with_label("Listener count URL (optional)")
        .build();
    let listeners_input = TextCtrl::builder(&panel).build();
    super::set_accessible_name(&listeners_input, "Listener count URL (optional)");
    super::help::tag(
        &listeners_input,
        "dialog.connect.icecastListeners",
        "Listener count URL for the selected service",
    );
    let username_label = StaticText::builder(&panel)
        .with_label("Icecast username")
        .build();
    let username_input = TextCtrl::builder(&panel).build();
    super::set_accessible_name(&username_input, "Icecast username");
    super::help::tag(
        &username_input,
        "dialog.connect.icecastUsername",
        "Icecast username for the selected service",
    );
    let icecast_password_label = StaticText::builder(&panel)
        .with_label("Icecast password")
        .build();
    let icecast_password_input = TextCtrl::builder(&panel)
        .with_style(TextCtrlStyle::Password)
        .build();
    super::set_accessible_name(&icecast_password_input, "Icecast password");
    super::help::tag(
        &icecast_password_input,
        "dialog.connect.icecastPassword",
        "Icecast password for the selected service",
    );

    let rtmp_url_label = StaticText::builder(&panel).with_label("Ingest URL").build();
    let rtmp_url_input = TextCtrl::builder(&panel).build();
    super::set_accessible_name(&rtmp_url_input, "Ingest URL");
    super::help::tag(
        &rtmp_url_input,
        "dialog.connect.rtmpUrl",
        "RTMP ingest URL for the selected service",
    );
    let rtmp_key_label = StaticText::builder(&panel).with_label("Stream key").build();
    // A password field, because that is what it is: anyone holding a stream key
    // can broadcast to the channel.
    let rtmp_key_input = TextCtrl::builder(&panel)
        .with_style(TextCtrlStyle::Password)
        .build();
    super::set_accessible_name(&rtmp_key_input, "Stream key");
    super::help::tag(
        &rtmp_key_input,
        "dialog.connect.rtmpKey",
        "RTMP stream key for the selected service",
    );
    let channel_label = StaticText::builder(&panel)
        .with_label("YouTube channel for chat (optional)")
        .build();
    let channel_input = TextCtrl::builder(&panel).build();
    super::set_accessible_name(&channel_input, "YouTube channel for chat (optional)");
    super::help::tag(
        &channel_input,
        "dialog.connect.youtubeChannel",
        "YouTube channel or video to read chat from",
    );
    let image_label = StaticText::builder(&panel)
        .with_label("Still image (optional)")
        .build();
    let image_input = TextCtrl::builder(&panel).build();
    super::set_accessible_name(&image_input, "Still image (optional)");
    super::help::tag(
        &image_input,
        "dialog.connect.youtubeImage",
        "Still image sent as the video track",
    );
    let browse_image = Button::builder(&panel).with_label("Choose image").build();
    super::help::tag(
        &browse_image,
        "dialog.connect.youtubeImageBrowse",
        "Choose still image button",
    );

    let connect_button = Button::builder(&panel).with_label("Connect").build();
    super::help::tag(
        &connect_button,
        "dialog.connect.connectButton",
        "Connect or disconnect button",
    );
    // `ID_CANCEL` is what wx maps Escape to, and it emulates a click, so
    // Escape saves the fields just like pressing Close does. `dismiss_button`
    // makes it the default item too, so Enter closes and saves as well.
    //
    // Connect deliberately stays off the default item: it starts or stops a live
    // connection, which is not something a stray Enter in the service list or a
    // password field should ever do.
    let close_button = super::dismiss_button(&panel, "Close");

    sizer.add(&services_label, 0, SizerFlag::All, 4);
    sizer.add(&services_list, 1, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add_sizer(&service_buttons, 0, SizerFlag::Expand, 0);
    sizer.add(&service_type, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&url_label, 0, SizerFlag::All, 4);
    sizer.add(&url_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&server_label, 0, SizerFlag::All, 4);
    sizer.add(&server_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&port_label, 0, SizerFlag::All, 4);
    sizer.add(&port_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&email_label, 0, SizerFlag::All, 4);
    sizer.add(&email_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&password_label, 0, SizerFlag::All, 4);
    sizer.add(&password_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&mount_label, 0, SizerFlag::All, 4);
    sizer.add(&mount_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&listeners_label, 0, SizerFlag::All, 4);
    sizer.add(&listeners_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&username_label, 0, SizerFlag::All, 4);
    sizer.add(&username_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&icecast_password_label, 0, SizerFlag::All, 4);
    sizer.add(
        &icecast_password_input,
        0,
        SizerFlag::Expand | SizerFlag::All,
        4,
    );
    sizer.add(&rtmp_url_label, 0, SizerFlag::All, 4);
    sizer.add(&rtmp_url_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&rtmp_key_label, 0, SizerFlag::All, 4);
    sizer.add(&rtmp_key_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&channel_label, 0, SizerFlag::All, 4);
    sizer.add(&channel_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&image_label, 0, SizerFlag::All, 4);
    sizer.add(&image_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&browse_image, 0, SizerFlag::All, 4);
    sizer.add(&connect_button, 0, SizerFlag::All, 8);
    sizer.add(&close_button, 0, SizerFlag::All, 8);
    panel.set_sizer(sizer, true);
    let dialog_sizer = BoxSizer::builder(Orientation::Vertical).build();
    dialog_sizer.add(&panel, 1, SizerFlag::Expand, 0);
    dialog.set_sizer(dialog_sizer, true);

    let update_field_visibility = {
        move || {
            let kind = service_type_at(service_type.get_selection());
            let audiopub = kind == StreamingServiceType::Audiopub;
            let icecast = kind == StreamingServiceType::Icecast;
            let youtube = kind == StreamingServiceType::Youtube;
            // The endpoint pair below is shared by the two Icecast-shaped types
            // and meaningless to an RTMP one, which carries its whole
            // destination in a single URL.
            let has_icecast_endpoint = audiopub || icecast;
            url_label.show(audiopub);
            url_input.show(audiopub);
            // These two fields serve both service types, so their wording
            // changes with the type - and the accessible name has to be re-set
            // alongside the visible label, or a screen reader goes on reading
            // the wording the field had when the dialog was built.
            let server_name = if audiopub {
                "Audiopub Icecast server"
            } else {
                "Icecast server"
            };
            let port_name = if audiopub {
                "Audiopub Icecast port"
            } else {
                "Icecast port"
            };
            server_label.set_label(server_name);
            super::set_accessible_name(&server_input, server_name);
            port_label.set_label(port_name);
            super::set_accessible_name(&port_input, port_name);
            email_label.show(audiopub);
            email_input.show(audiopub);
            password_label.show(audiopub);
            password_input.show(audiopub);
            server_label.show(has_icecast_endpoint);
            server_input.show(has_icecast_endpoint);
            port_label.show(has_icecast_endpoint);
            port_input.show(has_icecast_endpoint);
            mount_label.show(icecast);
            mount_input.show(icecast);
            // Audiopub counts its own listeners and reports them over the live
            // events feed, so this only applies to a direct Icecast service.
            // YouTube's arrive on its chat feed for the same reason.
            listeners_label.show(icecast);
            listeners_input.show(icecast);
            username_label.show(icecast);
            username_input.show(icecast);
            icecast_password_label.show(icecast);
            icecast_password_input.show(icecast);
            rtmp_url_label.show(youtube);
            rtmp_url_input.show(youtube);
            rtmp_key_label.show(youtube);
            rtmp_key_input.show(youtube);
            channel_label.show(youtube);
            channel_input.show(youtube);
            image_label.show(youtube);
            image_input.show(youtube);
            browse_image.show(youtube);
            panel.layout();
        }
    };

    // Refills the service list. `select_id` names the service the selection
    // should land on and is what marks the call a deliberate edit; `None`
    // relabels in place and leaves the selection where the user put it, which is
    // what the connection-state refreshes want - see `super::list`.
    let refresh_services = {
        let app = app.clone();
        move |select_id: Option<&str>| {
            let config = app.config.borrow();
            let services = &config.connection.sites;
            let labels: Vec<String> = services
                .iter()
                .map(|service| {
                    let connected =
                        app.run.borrow().connected_service.as_deref() == Some(service.id.as_str());
                    let mut label = format!(
                        "{} ({})",
                        service.display_name(),
                        service_type_name(service.service_type)
                    );
                    if service.is_main() {
                        label.push_str(" (main)");
                    }
                    if connected {
                        label.push_str(" (connected)");
                    }
                    label
                })
                .collect();
            let synced = super::list::sync(&services_list, &labels, NO_SERVICES);
            if services.is_empty() {
                return;
            }
            if select_id.is_none() && synced == super::list::Synced::Kept {
                return;
            }
            let select_index = services
                .iter()
                .position(|service| {
                    Some(service.id.as_str()) == select_id
                        || Some(service.url.as_str()) == select_id
                })
                .unwrap_or(0);
            services_list.set_selection(select_index as u32, true);
        }
    };

    // The id of the highlighted service, which is what the Connect button acts
    // on. `None` only for the placeholder row.
    let selected_service_id = {
        let app = app.clone();
        move || -> Option<String> {
            let config = app.config.borrow();
            let index = super::list::selection(&services_list, config.connection.sites.len())?;
            Some(config.connection.sites.get(index)?.id.clone())
        }
    };

    let update_actions = {
        let app = app.clone();
        move || {
            let config = app.config.borrow();
            let enabled = super::list::selection(&services_list, config.connection.sites.len())
                .and_then(|index| config.connection.sites.get(index))
                .map(|service| !service.is_main())
                .unwrap_or(false);
            rename_service.enable(enabled);
            remove_service.enable(enabled);
        }
    };

    let clear_fields = {
        move || {
            url_input.set_value("");
            email_input.set_value("");
            password_input.set_value("");
            server_input.set_value("");
            port_input.set_value("");
            mount_input.set_value("");
            listeners_input.set_value("");
            username_input.set_value("");
            icecast_password_input.set_value("");
            rtmp_url_input.set_value("");
            rtmp_key_input.set_value("");
            channel_input.set_value("");
            image_input.set_value("");
        }
    };

    let load_fields = {
        let app = app.clone();
        let update_actions = update_actions.clone();
        move || {
            let config = app.config.borrow();
            let Some(index) = super::list::selection(&services_list, config.connection.sites.len())
            else {
                clear_fields();
                update_actions();
                return;
            };
            if let Some(service) = config.connection.sites.get(index) {
                service_type.set_selection(service_type_index(service.service_type));
                service_type.enable(!service.is_main());
                url_input.set_value(&service.url);
                email_input.set_value(&service.email);
                password_input.set_value(service.password.as_str());
                // The effective endpoint, not the raw fields: an Audiopub
                // service that has never had one typed in publishes to the
                // site's usual host, and the dialog shows what will actually be
                // dialled rather than a blank the user has to guess at.
                let (server, port) = service.icecast_endpoint();
                server_input.set_value(&server);
                port_input.set_value(&port.to_string());
                mount_input.set_value(&service.icecast_mount);
                listeners_input.set_value(&service.icecast_listener_url);
                username_input.set_value(&service.icecast_username);
                icecast_password_input.set_value(service.icecast_password.as_str());
                rtmp_url_input.set_value(&service.rtmp_url);
                rtmp_key_input.set_value(service.rtmp_key.as_str());
                channel_input.set_value(&service.youtube_channel);
                image_input.set_value(&service.youtube_image);
            }
            drop(config);
            update_field_visibility();
            update_actions();
        }
    };

    // The button acts on the *highlighted* service, so it says which. It used
    // to read Disconnect whenever any connection existed, which made pressing
    // it on a service you had just selected disconnect the other one - and
    // since nothing refreshed the list either, the service you had left behind
    // went on claiming to be connected.
    let update_connect_label = {
        let app = app.clone();
        let selected_service_id = selected_service_id.clone();
        move || {
            let connected = app.run.borrow().connected_service.clone();
            let disconnects = connected.is_some() && connected == selected_service_id();
            connect_button.set_label(if disconnects {
                "Dis&connect"
            } else {
                "&Connect"
            });
        }
    };

    // Everything the connection state decides, in one call: the pump holds a
    // handle to this so a result arriving from the network thread updates the
    // dialog the same way pressing the button does.
    let sync_connection_ui: std::rc::Rc<dyn Fn()> = {
        let refresh_services = refresh_services.clone();
        let update_connect_label = update_connect_label.clone();
        std::rc::Rc::new(move || {
            refresh_services(None);
            update_connect_label();
        })
    };

    let initial_service = {
        let config = app.config.borrow();
        config
            .connection
            .last_used_site
            .clone()
            .unwrap_or_else(|| MAIN_SITE_URL.to_string())
    };
    refresh_services(Some(&initial_service));
    load_fields();
    update_connect_label();

    {
        let load_fields = load_fields.clone();
        let update_connect_label = update_connect_label.clone();
        services_list.clone().on_selection_changed(move |_| {
            load_fields();
            // Moving to another service changes what the button will do.
            update_connect_label();
        });
    }

    {
        service_type
            .clone()
            .on_selected(move |_| update_field_visibility());
    }

    // Save typed fields into the selected service on Connect and Close.
    let save_fields = {
        let app = app.clone();
        move || {
            let count = app.config.borrow().connection.sites.len();
            let index = super::list::selection(&services_list, count)?;
            let mut config = app.config.borrow_mut();
            let service = config.connection.sites.get_mut(index)?;
            if !service.is_main() {
                service.service_type = service_type_at(service_type.get_selection());
            } else {
                service.service_type = StreamingServiceType::Audiopub;
                service.nickname = "Audiopub".to_string();
                service.url = MAIN_SITE_URL.to_string();
            }
            service.url = url_input.get_value().trim().to_string();
            if service.is_main() {
                service.url = MAIN_SITE_URL.to_string();
            }
            service.email = email_input.get_value().trim().to_string();
            service.password = Secret::new(password_input.get_value());
            // A `host:port`, or a listen URL pasted whole, is unpacked into the
            // two fields it belongs in - so what is stored, and what the dialog
            // shows next time, is what will actually be dialled. Anything that
            // will not parse is kept verbatim for `service_profile_from_site` to
            // refuse with a message rather than being silently mangled here.
            let typed_server = server_input.get_value().trim().to_string();
            let typed_port: u16 = port_input.get_value().trim().parse().unwrap_or(0);
            let (server, port) = match crate::net::icecast::split_host_port(&typed_server) {
                Ok((host, embedded)) => (host, embedded.unwrap_or(typed_port)),
                Err(_) => (typed_server.clone(), typed_port),
            };
            service.icecast_server = server.clone();
            service.icecast_port = port;
            service.icecast_mount = mount_input.get_value().trim().to_string();
            // Stored verbatim, like the server field above: anything that will
            // not resolve is kept as typed for `service_profile_from_site` to
            // refuse with a message, rather than being silently mangled here.
            service.icecast_listener_url = listeners_input.get_value().trim().to_string();
            service.icecast_username = username_input.get_value().trim().to_string();
            service.icecast_password = Secret::new(icecast_password_input.get_value());
            service.rtmp_url = rtmp_url_input.get_value().trim().to_string();
            // Trimmed, because a stream key copied out of YouTube Studio very
            // often brings a trailing space or newline with it, and RTMP answers
            // that with the same closed socket a wrong key gets.
            service.rtmp_key = Secret::new(rtmp_key_input.get_value().trim());
            service.youtube_channel = channel_input.get_value().trim().to_string();
            service.youtube_image = image_input.get_value().trim().to_string();
            let id = service.id.clone();
            drop(config);
            // After the borrow, not during it: writing to a widget is a call out
            // into wx, and nothing in this closure may hold `config` across one.
            if server != typed_server {
                server_input.set_value(&server);
            }
            if port != typed_port {
                port_input.set_value(&port.to_string());
            }
            app.save_config();
            Some(id)
        }
    };

    {
        let app = app.clone();
        let dialog_for_add = dialog;
        let refresh_services = refresh_services.clone();
        let load_fields = load_fields.clone();
        add_service.on_click(move |_| {
            let Some((nickname, service_type)) = prompt_new_service(&dialog_for_add) else {
                return;
            };
            let id = {
                let mut config = app.config.borrow_mut();
                let id = config.connection.next_service_id();
                let service = match service_type {
                    StreamingServiceType::Audiopub => SiteConfig::audiopub(id.clone(), nickname),
                    StreamingServiceType::Icecast => SiteConfig::icecast(id.clone(), nickname),
                    StreamingServiceType::Youtube => SiteConfig::youtube(id.clone(), nickname),
                };
                config.connection.sites.push(service);
                id
            };
            app.save_config();
            refresh_services(Some(&id));
            load_fields();
        });
    }

    {
        let app = app.clone();
        let services_list_for_rename = services_list;
        let dialog_for_rename = dialog;
        let refresh_services = refresh_services.clone();
        rename_service.on_click(move |_| {
            let count = app.config.borrow().connection.sites.len();
            let Some(index) = super::list::selection(&services_list_for_rename, count) else {
                return;
            };
            let (id, current_name) = {
                let config = app.config.borrow();
                let Some(service) = config.connection.sites.get(index) else {
                    return;
                };
                if service.is_main() {
                    show_error(
                        &dialog_for_rename,
                        "Rename service",
                        "The main Audiopub service cannot be renamed.",
                    );
                    return;
                }
                (service.id.clone(), service.display_name())
            };
            let entry = TextEntryDialog::builder(
                &dialog_for_rename,
                "New service nickname:",
                "Rename service",
            )
            .with_default_value(&current_name)
            .build();
            if entry.show_modal() == ID_OK
                && let Some(name) = entry.get_value()
            {
                let name = name.trim();
                if name.is_empty() {
                    show_error(&dialog_for_rename, "Rename service", "Enter a nickname.");
                    return;
                }
                if let Some(service) = app.config.borrow_mut().connection.site_mut(&id) {
                    service.nickname = name.to_string();
                }
                app.save_config();
                refresh_services(Some(&id));
            }
        });
    }

    {
        let app = app.clone();
        let services_list_for_remove = services_list;
        let dialog_for_remove = dialog;
        let refresh_services = refresh_services.clone();
        let load_fields = load_fields.clone();
        remove_service.on_click(move |_| {
            let count = app.config.borrow().connection.sites.len();
            let Some(index) = super::list::selection(&services_list_for_remove, count) else {
                return;
            };
            {
                let mut config = app.config.borrow_mut();
                let Some(service) = config.connection.sites.get(index) else {
                    return;
                };
                if service.is_main() {
                    drop(config);
                    show_error(
                        &dialog_for_remove,
                        "Remove service",
                        "The main Audiopub service cannot be removed.",
                    );
                    return;
                }
                config.connection.sites.remove(index);
            }
            app.save_config();
            refresh_services(None);
            load_fields();
        });
    }

    {
        let dialog_for_image = dialog;
        browse_image.on_click(move |_| {
            let picker = FileDialog::builder(&dialog_for_image)
                .with_message("Choose the still image to send as video")
                .with_wildcard(
                    "Images (*.png;*.jpg;*.jpeg;*.bmp;*.gif)|*.png;*.jpg;*.jpeg;*.bmp;*.gif\
                     |All files (*.*)|*.*",
                )
                .with_style(FileDialogStyle::Open | FileDialogStyle::FileMustExist)
                .build();
            if picker.show_modal() == ID_OK
                && let Some(path) = picker.get_path()
            {
                image_input.set_value(&path);
            }
            // Not saved here: `save_fields` runs on Connect and on Close, and
            // writing the box is enough for both to pick it up.
        });
    }

    {
        let app = app.clone();
        let selected_service_id = selected_service_id.clone();
        let save_fields = save_fields.clone();
        let dialog_for_connect = dialog;
        connect_button.on_click(move |_| {
            let connected = app.run.borrow().connected_service.clone();
            // Only the connected service's own row disconnects. Pressing this
            // on any other service connects to that one instead, which the
            // network thread handles as a switch: it ends whatever stream is
            // live and replaces the connection.
            if connected.is_some() && connected == selected_service_id() {
                // Saved on the way out for the same reason Close saves: what is
                // typed in these fields is not lost by pressing a button.
                let _ = save_fields();
                app.net.send(NetCommand::Disconnect);
                // `connected_service`, the button label and the list's
                // "(connected)" marker are all left to the pump's `Disconnected`
                // arm, so one place decides what the dialog says.
                return;
            }
            let Some(id) = save_fields() else { return };
            let profile = {
                let config = app.config.borrow();
                let ffmpeg_path = config.connection.ffmpeg_path.clone();
                let Some(service) = config.connection.site(&id) else {
                    return;
                };
                match super::service_profile_from(service, &ffmpeg_path) {
                    Ok(profile) => profile,
                    Err(message) => {
                        drop(config);
                        show_error(&dialog_for_connect, "Connect", &message);
                        return;
                    }
                }
            };
            app.run.borrow_mut().connecting = true;
            app.net.send(NetCommand::Connect { profile });
            // Result arrives via the pump; the dialog may already be closed
            // by then, which is fine - the main window reports the outcome.
        });
    }

    {
        let dialog_for_close = dialog;
        let save_fields = save_fields.clone();
        close_button.on_click(move |_| {
            let _ = save_fields();
            dialog_for_close.end_modal(ID_CANCEL);
        });
    }

    // Register so connection results arriving on the pump report inside
    // this dialog and return focus to the connect button.
    *app.connect_ui.borrow_mut() = Some(super::ConnectUi {
        dialog,
        connect_button,
        sync: sync_connection_ui,
    });
    dialog.show_modal();
    *app.connect_ui.borrow_mut() = None;
    dialog.destroy();
}

fn prompt_new_service(parent: &Dialog) -> Option<(String, StreamingServiceType)> {
    let dialog = Dialog::builder(parent, "Add streaming service")
        .with_style(DialogStyle::DefaultDialogStyle)
        .with_size(360, 220)
        .build();
    let panel = Panel::builder(&dialog).build();
    let sizer = BoxSizer::builder(Orientation::Vertical).build();
    let nickname_label = StaticText::builder(&panel).with_label("Nickname").build();
    let nickname_input = TextCtrl::builder(&panel).build();
    super::set_accessible_name(&nickname_input, "Nickname");
    super::help::tag(
        &nickname_input,
        "dialog.connect.nickname",
        "Streaming service nickname",
    );
    let service_type = RadioBox::builder(&panel, &service_type_labels())
        .with_label("Service type")
        .with_style(RadioBoxStyle::SpecifyRows)
        .with_major_dimension(1)
        .build();
    service_type.set_selection(0);
    super::native_acc::install_radio_box(&service_type, "Service type");
    super::help::tag(
        &service_type,
        "dialog.connect.addServiceType",
        "New service type selector",
    );
    let buttons = BoxSizer::builder(Orientation::Horizontal).build();
    let ok = super::ok_button(&panel, "OK");
    let cancel = Button::builder(&panel)
        .with_id(ID_CANCEL)
        .with_label("Cancel")
        .build();
    {
        ok.on_click(move |_| dialog.end_modal(ID_OK));
    }
    {
        cancel.on_click(move |_| dialog.end_modal(ID_CANCEL));
    }
    buttons.add(&ok, 0, SizerFlag::All, 4);
    buttons.add(&cancel, 0, SizerFlag::All, 4);
    sizer.add(&nickname_label, 0, SizerFlag::All, 4);
    sizer.add(&nickname_input, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add(&service_type, 0, SizerFlag::Expand | SizerFlag::All, 4);
    sizer.add_sizer(&buttons, 0, SizerFlag::Expand, 0);
    panel.set_sizer(sizer, true);
    let dialog_sizer = BoxSizer::builder(Orientation::Vertical).build();
    dialog_sizer.add(&panel, 1, SizerFlag::Expand, 0);
    dialog.set_sizer(dialog_sizer, true);

    loop {
        if dialog.show_modal() != ID_OK {
            dialog.destroy();
            return None;
        }
        let nickname = nickname_input.get_value().trim().to_string();
        if nickname.is_empty() {
            show_error(&dialog, "Add service", "Enter a nickname.");
            continue;
        }
        let kind = service_type_at(service_type.get_selection());
        dialog.destroy();
        return Some((nickname, kind));
    }
}
