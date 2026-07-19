//! POC-COM GUI entry point: Mail / Social modes, both speaking real TTS
//! sentences over the same voice-channel-only audio path. No volume/gain
//! controls and no audio passthrough anywhere in this app -- see
//! `audio_io`'s crate docs for why that's structural, not just a UI choice.

use crate::mh_map::{Contact, ContactVia, MapMode, MapView};
use crate::pipeline::ReceivedPayload;
use crate::settings::Identity;
use crate::theme::{self, ACCENT, ERROR};
use crate::waterfall::WaterfallWindow;
use crate::{board_ui, listen, mail_ui, maidenhead, settings};
use std::time::SystemTime;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Mode {
    Mail,
    Social,
}

impl Mode {
    const ALL: [Mode; 2] = [Mode::Mail, Mode::Social];

    fn label(&self) -> &'static str {
        match self {
            Mode::Mail => "Mail",
            Mode::Social => "Social",
        }
    }
}

pub struct PocComApp {
    input_devices: Vec<audio_io::DeviceInfo>,
    output_devices: Vec<audio_io::DeviceInfo>,
    selected_input: Option<String>,
    selected_output: Option<String>,
    device_error: Option<String>,
    mode: Mode,
    mail_compose: mail_ui::ComposeState,
    mail_feed: mail_ui::InboxState,
    social_compose: board_ui::ComposeState,
    social_feed: board_ui::FeedState,
    /// One listening session shared across both modes -- there's only
    /// one input device, so there was never really two independent
    /// listening sessions, just two copies of the same code. Its decoded
    /// results get classified by kind and routed into whichever mode's
    /// list they belong to (see `route_received_payload` below),
    /// regardless of which tab is on screen when they arrive.
    listen: listen::ListenState,
    waterfall: WaterfallWindow,
    dark_mode: bool,
    /// Shared display name + home Maidenhead locator, set via the Settings
    /// popover and reused everywhere Mail's From/Location and Social's
    /// Author used to be typed separately. Persisted across restarts (see
    /// `settings.rs`) so this is genuinely "set once".
    identity: Identity,
    settings_open: bool,
    /// Camera state (pan/zoom) for the Settings popover's picker map --
    /// kept separate from the standalone map window's so opening one
    /// doesn't reset the other's view.
    settings_map_view: MapView,
    /// Available serial ports for the optional PTT picker (see
    /// `audio_io::ptt`) -- enumerated at startup and on demand via the
    /// Settings popover's refresh button, same pattern as the input/
    /// output audio device lists above.
    ptt_ports: Vec<String>,
    /// Held open only while the Settings popover's manual "Test PTT"
    /// button is toggled on -- lets a user confirm their port/baud
    /// actually keys the radio without having to prepare and send a real
    /// message first. Always released (see `settings_window`) when
    /// Settings closes or the port/baud selection changes underneath it,
    /// so a forgotten test can never leave the radio keyed indefinitely.
    ptt_test: Option<audio_io::PttPort>,
    ptt_test_error: Option<String>,
    map_window_open: bool,
    map_view: MapView,
    /// Every distinct locator a transmission has actually been received
    /// from, across Mail and Social both -- what the contact map draws
    /// lines out to. Deduplicated by locator (see `record_contact`).
    contacts: Vec<Contact>,
}

impl Default for PocComApp {
    fn default() -> Self {
        let (input_devices, output_devices, device_error) = match (audio_io::list_input_devices(), audio_io::list_output_devices()) {
            (Ok(inputs), Ok(outputs)) => (inputs, outputs, None),
            (in_res, out_res) => {
                let err = in_res.err().or(out_res.err()).map(|e| e.to_string());
                (Vec::new(), Vec::new(), err)
            }
        };
        let selected_input = input_devices.first().map(|d| d.name.clone());
        let selected_output = output_devices.first().map(|d| d.name.clone());
        let (waterfall_low, waterfall_high) = lexicon_modem::display_band();

        Self {
            input_devices,
            output_devices,
            selected_input,
            selected_output,
            device_error,
            mode: Mode::Mail,
            mail_compose: mail_ui::ComposeState::default(),
            mail_feed: mail_ui::InboxState::default(),
            social_compose: board_ui::ComposeState::default(),
            social_feed: board_ui::FeedState::default(),
            listen: listen::ListenState::default(),
            waterfall: WaterfallWindow::new(waterfall_low, waterfall_high),
            dark_mode: false,
            identity: settings::load(),
            settings_open: false,
            settings_map_view: MapView::default(),
            ptt_ports: audio_io::list_ptt_ports(),
            ptt_test: None,
            ptt_test_error: None,
            map_window_open: false,
            map_view: MapView::default(),
            contacts: Vec::new(),
        }
    }
}

impl PocComApp {
    /// A mode with PTT actually engaged (`Countdown`, `Transmitting`, or
    /// `Releasing`) shouldn't be silently interrupted by switching modes
    /// -- the radio may still be keyed up, and composing is still
    /// inherently mode-specific (you're authoring a payload as a
    /// specific type). Listening has no equivalent guard: it isn't tied
    /// to a mode anymore, so there's nothing mode-specific to interrupt
    /// by switching tabs.
    fn current_mode_is_transmitting(&self) -> bool {
        match self.mode {
            Mode::Mail => matches!(
                self.mail_compose.phase,
                mail_ui::SendPhase::Countdown { .. } | mail_ui::SendPhase::Transmitting { .. } | mail_ui::SendPhase::Releasing { .. }
            ),
            Mode::Social => matches!(
                self.social_compose.phase,
                board_ui::SendPhase::Countdown { .. } | board_ui::SendPhase::Transmitting { .. } | board_ui::SendPhase::Releasing { .. }
            ),
        }
    }

    /// Routes one decoded transmission into whichever mode's list matches
    /// its kind -- this is what makes the shared `listen` session a
    /// catch-all: it doesn't matter which tab was open when it arrived.
    fn route_received_payload(&mut self, payload: ReceivedPayload) {
        match payload {
            ReceivedPayload::Mail(mail) => {
                self.record_contact(&mail.from, &mail.location, ContactVia::Mail);
                self.mail_feed.push_message(mail);
            }
            ReceivedPayload::Social(social) => {
                if let Some(grid) = &social.origin_grid {
                    self.record_contact(&social.author, grid, ContactVia::Social);
                }
                self.social_feed.push_post(social.author, social.post, social.media_url);
            }
        }
    }

    /// Records (or refreshes) a contact for the map, keyed by locator so
    /// hearing from the same grid square repeatedly doesn't grow the list
    /// unbounded. Silently ignored if `locator` isn't a well-formed
    /// Maidenhead square -- covers legacy/freeform Location text from
    /// before this feature existed.
    fn record_contact(&mut self, label: &str, locator: &str, via: ContactVia) {
        if maidenhead::to_latlon_center(locator).is_none() {
            return;
        }
        let label = if label.trim().is_empty() { "(unknown)".to_string() } else { label.to_string() };
        if let Some(existing) = self.contacts.iter_mut().find(|c| c.locator.eq_ignore_ascii_case(locator)) {
            existing.label = label;
            existing.last_heard = SystemTime::now();
            existing.via = via;
        } else {
            self.contacts.push(Contact { locator: locator.to_string(), label, last_heard: SystemTime::now(), via });
        }
    }

    /// The Settings popover: display name, a click-to-pick home locator
    /// map, and an optional serial PTT port. There's no separate onboarding
    /// flow -- this is the same window `theme::identity_field` opens
    /// whether the identity is empty (first launch) or already set
    /// (changing it later). Picking a square on the map or a PTT port
    /// commits live (into `self.identity` directly) so the "Selected:"
    /// readout and the map's highlighted square always agree;
    /// `settings::save` persists to disk on any change and again on close.
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut still_open = true;
        let before = (self.identity.home_grid.clone(), self.identity.ptt_port.clone(), self.identity.ptt_baud);
        egui::Window::new("Settings")
            .id(egui::Id::new("settings_popover"))
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .default_pos(ctx.screen_rect().center() - egui::vec2(260.0, 220.0))
            .open(&mut still_open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Name / callsign:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.identity.display_name)
                            .char_limit(crate::pipeline::MAX_FROM_CHARS)
                            .hint_text("your callsign / name")
                            .desired_width(220.0),
                    );
                });
                ui.add_space(6.0);
                ui.label("Home locator -- scroll to zoom, drag to pan, click a square to pick it:");
                ui.add_space(4.0);
                crate::mh_map::map_ui(ui, &mut self.settings_map_view, MapMode::Pick { selected: &mut self.identity.home_grid }, 320.0);
                ui.add_space(6.0);
                ui.label(format!("Selected: {}", self.identity.home_grid.as_deref().unwrap_or("(none yet -- click the map)")));

                ui.add_space(14.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label("PTT (optional) -- auto-key a rig over a serial CAT port instead of pressing PTT by hand:");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("ptt_port")
                        .selected_text(self.identity.ptt_port.as_deref().unwrap_or("None (manual PTT)"))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.identity.ptt_port, None, "None (manual PTT)");
                            for port in &self.ptt_ports {
                                ui.selectable_value(&mut self.identity.ptt_port, Some(port.clone()), port);
                            }
                        });
                    if ui.button("⟳").on_hover_text("Re-scan serial ports").clicked() {
                        self.ptt_ports = audio_io::list_ptt_ports();
                    }
                    ui.add_space(10.0);
                    ui.label("Baud:");
                    egui::ComboBox::from_id_salt("ptt_baud")
                        .selected_text(self.identity.ptt_baud.to_string())
                        .show_ui(ui, |ui| {
                            for baud in audio_io::PTT_BAUD_RATES {
                                ui.selectable_value(&mut self.identity.ptt_baud, baud, baud.to_string());
                            }
                        });
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let port = self.identity.ptt_port.clone();
                    let testing = self.ptt_test.is_some();
                    let label = if testing { "⏹ Release PTT" } else { "🔘 Test PTT" };
                    if ui.add_enabled(port.is_some(), egui::Button::new(label)).on_hover_text("Manually key/unkey the selected port to confirm PTT actually works").clicked() {
                        if testing {
                            self.ptt_test = None;
                        } else if let Some(port) = &port {
                            self.ptt_test_error = None;
                            match audio_io::PttPort::key(port, self.identity.ptt_baud, std::time::Duration::ZERO) {
                                Ok(p) => self.ptt_test = Some(p),
                                Err(e) => self.ptt_test_error = Some(e.to_string()),
                            }
                        }
                    }
                    if testing {
                        theme::status_dot(ui, theme::SUCCESS, "PTT keyed -- click again to release");
                    }
                });
                if let Some(err) = &self.ptt_test_error {
                    ui.colored_label(ERROR, format!("⚠ {err}"));
                }
            });

        // A port/baud change invalidates whatever's currently keyed under
        // test -- release it rather than leave a stale connection open
        // pointing at settings that no longer match what's selected.
        if self.identity.ptt_port != before.1 || self.identity.ptt_baud != before.2 {
            self.ptt_test = None;
        }
        if (self.identity.home_grid.clone(), self.identity.ptt_port.clone(), self.identity.ptt_baud) != before {
            settings::save(&self.identity);
        }
        if !still_open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.settings_open = false;
            self.ptt_test = None;
            settings::save(&self.identity);
        }
    }

    /// The standalone contact map: a genuine second OS window (resizable,
    /// maximizable, draggable to another monitor) via egui's viewport API
    /// rather than an in-canvas popup. `show_viewport_immediate` (not
    /// `_deferred`) runs its closure synchronously inside this same
    /// `update()` call, so it can borrow `&mut self.map_view`/
    /// `&self.contacts` directly.
    fn map_window(&mut self, ctx: &egui::Context) {
        if !self.map_window_open {
            return;
        }
        let home = self.identity.home_grid.clone();
        let contacts = &self.contacts;
        let map_view = &mut self.map_view;
        let mut close_requested = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("contact_map"),
            egui::ViewportBuilder::default().with_title("POC-COM -- Contact Map").with_inner_size([900.0, 700.0]).with_resizable(true),
            |ctx, _class| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.heading("Contact Map");
                    ui.weak(if home.is_some() {
                        "House icon marks your home locator. Lines go out to everyone heard from."
                    } else {
                        "Set your home locator in Settings first."
                    });
                    ui.add_space(6.0);
                    let height = ui.available_height();
                    crate::mh_map::map_ui(ui, map_view, MapMode::View { home: home.as_deref(), contacts }, height);
                });
                if ctx.input(|i| i.viewport().close_requested()) {
                    close_requested = true;
                }
            },
        );

        if close_requested {
            self.map_window_open = false;
        }
    }
}

impl eframe::App for PocComApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.set_visuals(if self.dark_mode { egui::Visuals::dark() } else { egui::Visuals::light() });

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new("POC-COM").color(ACCENT).strong());
                ui.label(egui::RichText::new("Real speech over voice audio, transcribed back to text").weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let theme_icon = if self.dark_mode { "☀" } else { "🌙" };
                    let theme_hint = if self.dark_mode { "Switch to light theme" } else { "Switch to dark theme" };
                    if ui.button(theme_icon).on_hover_text(theme_hint).clicked() {
                        self.dark_mode = !self.dark_mode;
                    }
                    ui.add_space(6.0);
                    if ui.button("⚙").on_hover_text("Settings: name, home locator").clicked() {
                        self.settings_open = true;
                    }
                    ui.add_space(6.0);
                    if ui.selectable_label(self.map_window_open, "🗺 Map").on_hover_text("Open the contact map in its own window").clicked() {
                        self.map_window_open = !self.map_window_open;
                    }
                    ui.add_space(6.0);
                    if ui.selectable_label(self.waterfall.open, "📶 Waterfall").clicked() {
                        self.waterfall.toggle();
                    }
                });
            });
            ui.add_space(6.0);

            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("🎤 Input:");
                    egui::ComboBox::from_id_salt("input_device")
                        .selected_text(self.selected_input.as_deref().unwrap_or("(none)"))
                        .show_ui(ui, |ui| {
                            for d in &self.input_devices {
                                ui.selectable_value(&mut self.selected_input, Some(d.name.clone()), &d.name);
                            }
                        });

                    ui.add_space(20.0);

                    ui.label("🔊 Output:");
                    egui::ComboBox::from_id_salt("output_device")
                        .selected_text(self.selected_output.as_deref().unwrap_or("(none)"))
                        .show_ui(ui, |ui| {
                            for d in &self.output_devices {
                                ui.selectable_value(&mut self.selected_output, Some(d.name.clone()), &d.name);
                            }
                        });
                });
                if let Some(err) = &self.device_error {
                    ui.colored_label(ERROR, format!("Device enumeration error: {err}"));
                }
            });
            ui.add_space(8.0);

            let transmitting = self.current_mode_is_transmitting();
            ui.horizontal(|ui| {
                for candidate in Mode::ALL {
                    let selected = self.mode == candidate;
                    let enabled = selected || !transmitting;
                    if ui.add_enabled(enabled, egui::SelectableLabel::new(selected, egui::RichText::new(candidate.label()).size(15.0))).clicked() {
                        self.mode = candidate;
                    }
                }
                if transmitting {
                    ui.weak("Finish or cancel the current transmission to switch modes.");
                }
            });
            ui.add_space(4.0);
        });

        if self.waterfall.open {
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.waterfall.open = false;
            } else {
                let waterfall = &mut self.waterfall;
                egui::SidePanel::right("waterfall_panel").resizable(true).default_width(420.0).width_range(300.0..=900.0).show(ctx, |ui| {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.heading("Waterfall");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(egui::RichText::new("X").strong()).on_hover_text("Close (Esc)").clicked() {
                                waterfall.open = false;
                            }
                        });
                    });
                    ui.add_space(4.0);
                    if waterfall.open {
                        // This scheme only ever speaks one word at a time
                        // (real speech has no parallel-tone-lane
                        // equivalent), so there's always exactly one
                        // stream for the waterfall to show.
                        waterfall.content(ui, 1);
                    }
                });
            }
        }

        // One shared panel id across both modes so the user's chosen
        // split stays put when switching modes, rather than each mode
        // remembering its own width independently.
        egui::SidePanel::left("compose_panel").resizable(true).default_width(420.0).width_range(340.0..=800.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().id_salt("compose_scroll").show(ui, |ui| match self.mode {
                Mode::Mail => mail_ui::compose_ui(
                    ui,
                    ctx,
                    &mut self.mail_compose,
                    self.selected_output.as_deref(),
                    &mut self.waterfall,
                    &self.identity,
                ),
                Mode::Social => board_ui::compose_ui(
                    ui,
                    ctx,
                    &mut self.social_compose,
                    self.selected_output.as_deref(),
                    &mut self.waterfall,
                    &self.identity,
                ),
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // Drain before rendering so the pending-decode count shown
            // below reflects this frame's drain, then route each result
            // into its matching mode's list before that mode's own list
            // renders -- a message decoded while sitting on the Social tab
            // still needs to land in the Mail tab's inbox this same frame.
            for payload in listen::drain_pending(&mut self.listen) {
                self.route_received_payload(payload);
            }
            listen::listen_status_ui(ui, ctx, &mut self.listen, self.selected_input.as_deref(), &mut self.waterfall);

            ui.add_space(10.0);

            match self.mode {
                Mode::Mail => mail_ui::inbox_ui(ui, ctx, &mut self.mail_feed, &self.identity.display_name),
                Mode::Social => board_ui::feed_ui(ui, ctx, &mut self.social_feed),
            }
        });

        self.settings_window(ctx);
        self.map_window(ctx);
    }
}
