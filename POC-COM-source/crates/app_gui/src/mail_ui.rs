//! Email-client-style UI for POC-COM_MAIL: a compose panel (left) for
//! authoring a single plain-text mail message and an inbox panel (right)
//! that continuously listens and lists what's been received, email-client
//! style. Both panels share the app's single embedded waterfall (see
//! `waterfall.rs`), fed from whichever side is currently active
//! (transmitting or listening).
//!
//! A message is just From/To/Location/Subject/Message spoken aloud as a
//! real sentence (see `lexicon_modem::message::build_mail_text`) -- no
//! attachment, no compression, nothing binary. Anyone listening on the
//! radio hears exactly what lands in the inbox.

use crate::pipeline::{prepare_mail_transmission, PreparedTx};
use crate::settings::Identity;
use crate::theme::{self, ACCENT, ERROR, WARNING};
use crate::wav;
use crate::waterfall::WaterfallWindow;
use lexicon_modem::message::MailFields;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PTT_LEAD_IN: Duration = Duration::from_secs(3);
/// Wait after keying a serial PTT port for the radio's relay to
/// physically close before audio starts -- mirrors MMSSTV's own
/// `TX;\w10` command template, just longer (confirmed via real testing
/// that both 10ms and 250ms cut it too close for comfort). See
/// `audio_io::ptt`.
const PTT_PRE_DELAY: Duration = Duration::from_millis(750);
/// Held after the audio itself finishes, before PTT actually releases --
/// covers OS/soundcard output buffering, which can report "finished"
/// slightly before the last samples have physically left the speaker.
const PTT_POST_DELAY: Duration = Duration::from_millis(400);

pub enum SendPhase {
    Idle,
    Preparing { rx: mpsc::Receiver<Result<PreparedTx, String>> },
    /// Synthesized, sitting here until the user picks Send or Save as WAV
    /// (or both -- this doesn't consume the waveform).
    Prepared { prepared: PreparedTx },
    Countdown { prepared: PreparedTx, deadline: Instant },
    /// `ptt` is `None` unless a serial PTT port is configured in
    /// Settings -- when present, it's held open for the whole
    /// transmission (and through `Releasing` below).
    Transmitting { tx: audio_io::TxHandle, ptt: Option<audio_io::PttPort> },
    /// Audio has finished playing but a configured PTT connection is
    /// held keyed a little longer (`PTT_POST_DELAY`) before its `Drop`
    /// impl actually sends the Kenwood `RX;` key-down command --
    /// `_ptt` is only ever held for that side effect, never read
    /// directly (skipped entirely when `None`, since manual PTT has
    /// nothing here to hold open).
    Releasing { until: Instant, _ptt: Option<audio_io::PttPort> },
    Done,
    Failed(String),
}

pub struct ComposeState {
    pub to: String,
    pub subject: String,
    pub message: String,
    pub phase: SendPhase,
    /// Cloned off `PreparedTx` the moment it's ready, since the wave itself
    /// gets moved into `audio_io::start_transmission` once Countdown ends --
    /// the waterfall needs its own copy to keep drawing from during and
    /// after playback.
    cached_wave: Option<(Vec<f32>, u32)>,
    /// Set if a configured serial PTT port failed to key at the moment a
    /// transmission started -- the transmission still proceeds (manual
    /// PTT is always the fallback), this is just surfaced so a silently
    /// unkeyed radio doesn't go unnoticed.
    ptt_warning: Option<String>,
}

impl Default for ComposeState {
    fn default() -> Self {
        Self {
            to: String::new(),
            subject: String::new(),
            message: String::new(),
            phase: SendPhase::Idle,
            cached_wave: None,
            ptt_warning: None,
        }
    }
}

impl ComposeState {
    /// Clears the per-message fields (subject/message) after a send, but
    /// keeps To -- like a real mail client remembering who you were
    /// writing to between messages.
    fn reset_for_next_message(&mut self) {
        self.subject.clear();
        self.message.clear();
        self.phase = SendPhase::Idle;
        self.cached_wave = None;
    }
}

pub fn compose_ui(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    state: &mut ComposeState,
    output_device: Option<&str>,
    waterfall: &mut WaterfallWindow,
    identity: &Identity,
) {
    ui.heading("📡 Compose");
    ui.add_space(6.0);

    theme::card(ui, |ui| {
        egui::Grid::new("compose_fields").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("From:");
            theme::identity_field(ui, Some(identity.display_name.as_str()));
            ui.end_row();

            ui.label("To:");
            ui.add(
                egui::TextEdit::singleline(&mut state.to)
                    .char_limit(crate::pipeline::MAX_TO_CHARS)
                    .hint_text("recipient callsign / name")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("Location:");
            theme::identity_field(ui, identity.home_grid.as_deref());
            ui.end_row();

            ui.label("Subject:");
            ui.add(
                egui::TextEdit::singleline(&mut state.subject)
                    .char_limit(crate::pipeline::MAX_SUBJECT_CHARS)
                    .hint_text("subject")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();
        });
    });

    ui.add_space(8.0);
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Message:");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(format!("{}/{} chars", state.message.chars().count(), crate::pipeline::MAX_MESSAGE_CHARS));
            });
        });
        ui.add(
            egui::TextEdit::multiline(&mut state.message)
                .char_limit(crate::pipeline::MAX_MESSAGE_CHARS)
                .desired_rows(10)
                .desired_width(f32::INFINITY),
        );
    });

    ui.add_space(10.0);

    let identity_ready = !identity.display_name.trim().is_empty() && identity.home_grid.is_some();
    let can_send = !state.message.trim().is_empty();
    let can_prepare = can_send && identity_ready && matches!(state.phase, SendPhase::Idle | SendPhase::Done | SendPhase::Failed(_));

    if ui.add_enabled(can_prepare, egui::Button::new("Prepare Message")).clicked() {
        let from = identity.display_name.clone();
        let to = state.to.clone();
        let location = identity.home_grid.clone().unwrap_or_default();
        let subject = state.subject.clone();
        let message = state.message.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(Ok(prepare_mail_transmission(&from, &to, &location, &subject, &message)));
        });
        state.phase = SendPhase::Preparing { rx };
    }
    if !identity_ready {
        ui.weak("Set your callsign and home locator in Settings (⚙) before sending.");
    } else if !can_send {
        ui.weak("Write a message before sending.");
    }

    ui.add_space(10.0);

    match &mut state.phase {
        SendPhase::Idle => {}
        SendPhase::Preparing { rx } => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Assembling audio...");
            });
            if let Ok(result) = rx.try_recv() {
                state.phase = match result {
                    Ok(prepared) => {
                        state.cached_wave = Some((prepared.wave.clone(), prepared.sample_rate));
                        SendPhase::Prepared { prepared }
                    }
                    Err(e) => SendPhase::Failed(e),
                };
            }
            ctx.request_repaint();
        }
        SendPhase::Prepared { prepared } => {
            let mut transmit_clicked = false;
            let mut save_error: Option<String> = None;
            let mut edit_clicked = false;
            theme::card(ui, |ui| {
                theme::status_dot(
                    ui,
                    ACCENT,
                    &format!(
                        "Ready: {} of speech at {} Hz",
                        format_duration(prepared.wave.len() as f32 / prepared.sample_rate as f32),
                        prepared.sample_rate
                    ),
                );
                ui.add_space(4.0);
                ui.label(egui::RichText::new(format!("\"{}\"", prepared.spoken_text)).weak().italics());
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(output_device.is_some(), egui::Button::new("📡 Send")).clicked() {
                        transmit_clicked = true;
                    }
                    if ui.button("💾 Save as WAV...").clicked() {
                        if let Some(path) =
                            rfd::FileDialog::new().set_file_name("poc_com_mail_message.wav").add_filter("WAV audio", &["wav"]).save_file()
                        {
                            if let Err(e) = wav::write_wav_f32(&path, prepared.sample_rate, &prepared.wave) {
                                save_error = Some(format!("couldn't save WAV: {e}"));
                            }
                        }
                    }
                    if ui.button("Edit").on_hover_text("Go back and change the message before sending").clicked() {
                        edit_clicked = true;
                    }
                });
            });

            if transmit_clicked {
                let SendPhase::Prepared { prepared } = std::mem::replace(&mut state.phase, SendPhase::Idle) else {
                    unreachable!()
                };
                state.phase = SendPhase::Countdown { prepared, deadline: Instant::now() + PTT_LEAD_IN };
            } else if edit_clicked {
                state.phase = SendPhase::Idle;
                state.cached_wave = None;
            } else if let Some(err) = save_error {
                state.phase = SendPhase::Failed(err);
            }
        }
        SendPhase::Countdown { deadline, .. } => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            theme::card(ui, |ui| {
                let heading = if identity.ptt_port.is_some() {
                    format!("🎙 PTT will key automatically -- starting in {:.0}s", remaining.as_secs_f32().ceil())
                } else {
                    format!("🎙 Press and hold PTT now -- starting in {:.0}s", remaining.as_secs_f32().ceil())
                };
                ui.heading(heading);
            });
            if remaining.is_zero() {
                let SendPhase::Countdown { prepared, .. } =
                    std::mem::replace(&mut state.phase, SendPhase::Failed("internal state error".into()))
                else {
                    unreachable!()
                };
                let ptt = match identity.ptt_port.as_deref() {
                    Some(port) => match audio_io::PttPort::key(port, identity.ptt_baud, PTT_PRE_DELAY) {
                        Ok(p) => {
                            state.ptt_warning = None;
                            Some(p)
                        }
                        Err(e) => {
                            state.ptt_warning = Some(e.to_string());
                            None
                        }
                    },
                    None => None,
                };
                match audio_io::start_transmission(output_device.unwrap_or_default(), prepared.wave, prepared.sample_rate) {
                    Ok(tx) => state.phase = SendPhase::Transmitting { tx, ptt },
                    Err(e) => state.phase = SendPhase::Failed(e.to_string()),
                }
            }
            ctx.request_repaint();
        }
        SendPhase::Transmitting { tx, ptt } => {
            theme::card(ui, |ui| {
                let heading = if ptt.is_some() { "📡 Sending -- PTT keyed automatically" } else { "📡 Sending -- keep holding PTT" };
                ui.heading(heading);
                ui.add_space(4.0);
                ui.add(egui::ProgressBar::new(tx.progress()).show_percentage());
                if ui.button("Cancel").clicked() {
                    tx.cancel();
                }
            });
            if let Some(warning) = &state.ptt_warning {
                ui.colored_label(WARNING, format!("⚠ PTT port didn't respond, sending with manual PTT: {warning}"));
            }
            if tx.is_finished() {
                let SendPhase::Transmitting { ptt, .. } = std::mem::replace(&mut state.phase, SendPhase::Done) else { unreachable!() };
                state.phase = match ptt {
                    Some(ptt) => SendPhase::Releasing { until: Instant::now() + PTT_POST_DELAY, _ptt: Some(ptt) },
                    None => SendPhase::Done,
                };
            }
            ctx.request_repaint();
        }
        SendPhase::Releasing { until, .. } => {
            theme::card(ui, |ui| {
                theme::status_dot(ui, ACCENT, "📡 Sent -- releasing PTT...");
            });
            if Instant::now() >= *until {
                state.phase = SendPhase::Done;
            }
            ctx.request_repaint();
        }
        SendPhase::Done => {
            let mut new_message_clicked = false;
            theme::card(ui, |ui| {
                theme::status_dot(ui, theme::SUCCESS, "Message sent. You can release PTT now.");
                ui.add_space(4.0);
                if ui.button("New Message").clicked() {
                    new_message_clicked = true;
                }
            });
            if new_message_clicked {
                state.reset_for_next_message();
            }
        }
        SendPhase::Failed(err) => {
            let mut try_again_clicked = false;
            theme::card(ui, |ui| {
                theme::status_dot(ui, ERROR, &format!("Failed: {err}"));
                ui.add_space(4.0);
                if ui.button("Try Again").clicked() {
                    try_again_clicked = true;
                }
            });
            if try_again_clicked {
                state.phase = SendPhase::Idle;
            }
        }
    }

    // Feed the waterfall from wherever playback currently is in the cached
    // waveform. Outside `Transmitting` there's nothing actively "playing",
    // so it just holds its last frame rather than animating. `app.rs` owns
    // actually drawing the panel (it's embedded, shared chrome with the
    // Inbox's).
    if let SendPhase::Transmitting { tx, .. } = &state.phase {
        if waterfall.wants_feed() {
            if let Some((wave, rate)) = &state.cached_wave {
                let pos = ((tx.progress() * wave.len() as f32) as usize).min(wave.len());
                let window_start = pos.saturating_sub(WaterfallWindow::FEED_WINDOW_SAMPLES);
                waterfall.feed(&wave[window_start..pos.max(window_start)], *rate);
            }
        }
    }
}

pub struct InboxItem {
    pub message: MailFields,
    pub received_at: SystemTime,
    pub read: bool,
}

pub struct InboxState {
    pub items: Vec<InboxItem>,
    /// Index of the item currently shown in the reading popover, if any.
    reading: Option<usize>,
    /// Multi-select checkboxes for bulk delete, kept parallel to `items`.
    checked: Vec<bool>,
    /// When enabled, only messages whose To: field contains this text
    /// (case-insensitive) are shown -- "only what's addressed to me".
    pub filter_to_enabled: bool,
    pub filter_to: String,
    /// When enabled, only messages whose From: field contains this text
    /// (case-insensitive) are shown -- "only what a given sender sent".
    pub filter_from_enabled: bool,
    pub filter_from: String,
    /// Set for one frame when a new message lands, so the list can jump
    /// back to the top (where the newest item always is) even if the user
    /// had scrolled down to read older ones.
    just_received: bool,
}

impl Default for InboxState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            reading: None,
            checked: Vec::new(),
            filter_to_enabled: false,
            filter_to: String::new(),
            filter_from_enabled: false,
            filter_from: String::new(),
            just_received: false,
        }
    }
}

impl InboxState {
    /// Called by `app.rs` after the shared listening session (see
    /// `listen.rs`) decodes a `Mail` payload -- listening itself isn't
    /// mode-specific anymore, so this is the only entry point for new mail
    /// landing here regardless of which tab was on screen when it arrived.
    pub fn push_message(&mut self, message: MailFields) {
        self.items.insert(0, InboxItem { message, received_at: SystemTime::now(), read: false });
        self.checked.insert(0, false);
        // Keep the reading popover pointing at the same logical item now
        // that a new one was inserted ahead of it.
        self.reading = self.reading.map(|s| s + 1);
        self.just_received = true;
    }

    /// Removes a single item by index and keeps `checked`/`reading` in sync.
    fn delete_at(&mut self, i: usize) {
        if i >= self.items.len() {
            return;
        }
        self.items.remove(i);
        self.checked.remove(i);
        self.reading = match self.reading {
            Some(r) if r == i => None,
            Some(r) if r > i => Some(r - 1),
            other => other,
        };
    }

    fn delete_checked(&mut self) {
        let mut i = 0;
        while i < self.items.len() {
            if self.checked[i] {
                self.items.remove(i);
                self.checked.remove(i);
            } else {
                i += 1;
            }
        }
        // Bulk deletes can shuffle indices in ways that are fiddly to track
        // precisely -- simplest and safest is to just close the popover.
        self.reading = None;
    }
}

pub fn inbox_ui(ui: &mut egui::Ui, ctx: &egui::Context, state: &mut InboxState, my_identity: &str) {
    ui.heading("Inbox");
    ui.add_space(6.0);

    theme::card(ui, |ui| {
        let checked_count = state.checked.iter().filter(|&&c| c).count();
        ui.horizontal(|ui| {
            if ui.add_enabled(checked_count > 0, egui::Button::new(format!("Delete ({checked_count})"))).clicked() {
                state.delete_checked();
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.checkbox(&mut state.filter_to_enabled, "To:");
            ui.add_enabled(
                state.filter_to_enabled,
                egui::TextEdit::singleline(&mut state.filter_to).desired_width(100.0).hint_text("recipient"),
            );
            if ui.add_enabled(!my_identity.trim().is_empty(), egui::Button::new("Only mine")).on_hover_text("Filter To: to my From: identity").clicked() {
                state.filter_to = my_identity.to_string();
                state.filter_to_enabled = true;
            }
            ui.add_space(10.0);
            ui.checkbox(&mut state.filter_from_enabled, "From:");
            ui.add_enabled(
                state.filter_from_enabled,
                egui::TextEdit::singleline(&mut state.filter_from).desired_width(100.0).hint_text("sender"),
            );
        });
    });

    ui.add_space(6.0);

    // No inline reading pane anymore (see the popover below), so the list
    // gets essentially all remaining vertical space in this column.
    let list_height = (ui.available_height() - 4.0).max(120.0);
    let filter_to = state.filter_to.to_lowercase();
    let filter_from = state.filter_from.to_lowercase();
    let filter_to_enabled = state.filter_to_enabled;
    let filter_from_enabled = state.filter_from_enabled;
    let any_filter_active = filter_to_enabled || filter_from_enabled;

    let mut pending_open: Option<usize> = None;
    let mut pending_delete: Option<usize> = None;

    let mut scroll_area = egui::ScrollArea::vertical().id_salt("inbox_list").max_height(list_height);
    if state.just_received {
        scroll_area = scroll_area.vertical_scroll_offset(0.0);
        state.just_received = false;
    }
    scroll_area.show(ui, |ui| {
        if state.items.is_empty() {
            ui.weak("No messages received yet.");
        } else if any_filter_active && !state.items.iter().any(|item| {
            (!filter_to_enabled || item.message.to.to_lowercase().contains(&filter_to))
                && (!filter_from_enabled || item.message.from.to_lowercase().contains(&filter_from))
        }) {
            ui.weak("No messages match the current filter.");
        }
        for i in 0..state.items.len() {
            let item = &state.items[i];
            let to_ok = !filter_to_enabled || item.message.to.to_lowercase().contains(&filter_to);
            let from_ok = !filter_from_enabled || item.message.from.to_lowercase().contains(&filter_from);
            if !(to_ok && from_ok) {
                continue;
            }

            let is_reading = state.reading == Some(i);
            let subject = if item.message.subject.is_empty() { "(no subject)" } else { item.message.subject.as_str() };
            let row_text = format!(
                "{}{}\n{} · {}",
                if item.read { "" } else { "● " },
                subject,
                display_or_dash(&item.message.from),
                format_hms(item.received_at),
            );
            let mut text = egui::RichText::new(row_text);
            if !item.read {
                text = text.strong();
            }

            ui.horizontal(|ui| {
                ui.checkbox(&mut state.checked[i], "");
                if ui.selectable_label(is_reading, text).clicked() {
                    pending_open = Some(i);
                }
                if ui.add(egui::Button::new(egui::RichText::new("X").small())).on_hover_text("Delete this message").clicked() {
                    pending_delete = Some(i);
                }
            });
        }
    });

    if let Some(i) = pending_delete {
        state.delete_at(i);
    } else if let Some(i) = pending_open {
        state.reading = Some(i);
        state.items[i].read = true;
    }

    // Reading popover: pulled out of the list so the middle column stays
    // free for as many inbox rows as will fit. Closes on the built-in
    // window X (via `.open()`) or on Esc.
    if let Some(i) = state.reading {
        let Some(item) = state.items.get(i) else {
            state.reading = None;
            return;
        };
        let msg = &item.message;
        let title = if msg.subject.is_empty() { "(no subject)".to_string() } else { msg.subject.clone() };
        let mut still_open = true;

        egui::Window::new(title)
            .id(egui::Id::new("mail_reading_popover"))
            .collapsible(false)
            .resizable(true)
            .default_width(420.0)
            .default_pos(ctx.screen_rect().center() - egui::vec2(210.0, 160.0))
            .open(&mut still_open)
            .show(ctx, |ui| {
                egui::Grid::new("reading_popover_fields").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                    ui.weak("From:");
                    ui.label(display_or_dash(&msg.from));
                    ui.end_row();
                    ui.weak("To:");
                    ui.label(display_or_dash(&msg.to));
                    ui.end_row();
                    ui.weak("Location:");
                    ui.label(display_or_dash(&msg.location));
                    ui.end_row();
                    ui.weak("Received:");
                    ui.label(format_hms(item.received_at));
                    ui.end_row();
                });
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);
                egui::ScrollArea::vertical().id_salt("reading_popover_body").max_height(260.0).show(ui, |ui| {
                    ui.label(&msg.message);
                });
            });

        if !still_open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.reading = None;
        }
    }
}

fn display_or_dash(s: &str) -> &str {
    if s.trim().is_empty() {
        "--"
    } else {
        s
    }
}

fn format_hms(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02} UTC")
}

fn format_duration(seconds: f32) -> String {
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{:.0}m{:02.0}s", (seconds / 60.0).floor(), seconds % 60.0)
    }
}
