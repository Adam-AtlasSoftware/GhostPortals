//! The manager window: start and stop the daemon, see what it is doing,
//! change settings, run the self-test and install updates.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui::{self, RichText};

use crate::daemon::{self, Daemon};
use crate::install;
use crate::settings::Settings;
use crate::style;
use crate::update::{self, Release};

#[derive(Clone)]
#[cfg_attr(not(windows), allow(dead_code))]
enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(Box<Release>),
    Downloading { got: Arc<AtomicU64>, total: u64 },
    Installed,
    Failed(String),
}

#[derive(Clone)]
enum TestState {
    Idle,
    Running,
    Done(Result<String, String>),
}

pub struct Manager {
    settings: Settings,
    /// Settings as edited, applied with the Apply button.
    draft: Settings,
    /// Whether the draft dials the other computer (else it listens).
    dial: bool,
    autostart: bool,
    daemon: Daemon,
    update: Arc<Mutex<UpdateState>>,
    test: Arc<Mutex<TestState>>,
    error: Option<String>,
    show_log: bool,
}

impl Manager {
    pub fn new(cc: &eframe::CreationContext<'_>, minimized: bool) -> Self {
        style::apply(&cc.egui_ctx);
        let settings = Settings::load();
        let mut app = Self {
            dial: !settings.connect.is_empty(),
            draft: settings.clone(),
            settings,
            autostart: install::autostart_enabled(),
            daemon: Daemon::new(),
            update: Arc::new(Mutex::new(UpdateState::Idle)),
            test: Arc::new(Mutex::new(TestState::Idle)),
            error: None,
            show_log: false,
        };
        if app.settings.autostart_daemon || minimized {
            app.start(&cc.egui_ctx);
        }
        if app.settings.check_updates {
            app.check_updates(&cc.egui_ctx);
        }
        if minimized {
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        app
    }

    fn start(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        self.error = self
            .daemon
            .start(&self.settings, move || ctx.request_repaint())
            .err()
            .map(|e| format!("{e:#}"));
    }

    fn check_updates(&self, ctx: &egui::Context) {
        let state = self.update.clone();
        let ctx = ctx.clone();
        *state.lock().unwrap() = UpdateState::Checking;
        std::thread::spawn(move || {
            let next = match update::latest() {
                Ok(r) if r.version().is_some_and(|v| v > update::current()) => {
                    UpdateState::Available(Box::new(r))
                }
                Ok(_) => UpdateState::UpToDate,
                Err(e) => UpdateState::Failed(format!("{e:#}")),
            };
            *state.lock().unwrap() = next;
            ctx.request_repaint();
        });
    }

    #[cfg(windows)]
    fn install_update(&mut self, ctx: &egui::Context, release: Release) {
        let Some(asset) = release.asset(update::WINDOWS_ASSET).cloned() else {
            *self.update.lock().unwrap() =
                UpdateState::Failed("the release has no Windows bundle".into());
            return;
        };
        self.daemon.stop();
        let got = Arc::new(AtomicU64::new(0));
        *self.update.lock().unwrap() =
            UpdateState::Downloading { got: got.clone(), total: asset.size };
        let state = self.update.clone();
        let ctx = ctx.clone();
        let version = release.tag_name.trim_start_matches('v').to_owned();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let zip = std::env::temp_dir().join(format!("ghostportals-{version}.zip"));
                update::download(&asset, &zip, &got)?;
                let opts = install::Options {
                    desktop_shortcut: false,
                    start_on_login: install::autostart_enabled(),
                };
                install::install(&zip, &install::install_dir(), &version, opts)?;
                let _ = std::fs::remove_file(&zip);
                Ok(())
            })();
            *state.lock().unwrap() = match result {
                Ok(()) => UpdateState::Installed,
                Err(e) => UpdateState::Failed(format!("{e:#}")),
            };
            ctx.request_repaint();
        });
    }

    fn run_selftest(&self, ctx: &egui::Context) {
        let state = self.test.clone();
        let settings = self.settings.clone();
        let ctx = ctx.clone();
        *state.lock().unwrap() = TestState::Running;
        std::thread::spawn(move || {
            let result = daemon::selftest(&settings).map_err(|e| format!("{e:#}"));
            *state.lock().unwrap() = TestState::Done(result);
            ctx.request_repaint();
        });
    }

    fn update_banner(&mut self, ui: &mut egui::Ui) {
        let state = self.update.lock().unwrap().clone();
        match state {
            UpdateState::Available(release) => {
                style::banner(ui, style::ACCENT, |ui| {
                    ui.label(
                        RichText::new(format!("Version {} is available", release.tag_name))
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        #[cfg(windows)]
                        if install::is_installed_copy() {
                            if ui.button("Update now").clicked() {
                                self.install_update(ui.ctx(), *release.clone());
                            }
                        } else {
                            ui.label(RichText::new("(not the installed copy)").weak());
                        }
                        if ui.button("Release notes").clicked() {
                            let _ = open::that(&release.html_url);
                        }
                    });
                });
            }
            UpdateState::Downloading { got, total } => {
                style::banner(ui, style::ACCENT, |ui| {
                    let got = got.load(Ordering::Relaxed);
                    ui.label("Downloading update…");
                    ui.add(egui::ProgressBar::new(got as f32 / total.max(1) as f32).text(format!(
                        "{:.1} / {:.1} MB",
                        got as f64 / 1e6,
                        total as f64 / 1e6
                    )));
                });
            }
            UpdateState::Installed => {
                style::banner(ui, style::GOOD, |ui| {
                    ui.label(RichText::new("Update installed.").strong());
                    if ui.button("Restart Ghost Portals").clicked() {
                        let _ = std::process::Command::new(install::current_exe()).spawn();
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            }
            UpdateState::Failed(e) => {
                style::banner(ui, style::WARN, |ui| {
                    ui.label(format!("Update check failed: {e}"));
                });
            }
            UpdateState::Idle | UpdateState::Checking | UpdateState::UpToDate => {}
        }
    }

    fn status_card(&mut self, ui: &mut egui::Ui) {
        let running = self.daemon.running();
        let status = self.daemon.status();
        style::card(ui, |ui| {
            ui.horizontal(|ui| {
                let (dot, text) = match (running, &status.peer) {
                    (true, Some(_)) => (style::GOOD, "Connected"),
                    (true, None) => (style::ACCENT, "Waiting for the other computer"),
                    (false, _) => (style::MUTED, "Stopped"),
                };
                style::dot(ui, dot);
                ui.label(RichText::new(text).size(18.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if running {
                        if ui.button("Stop").clicked() {
                            self.daemon.stop();
                        }
                    } else if ui
                        .add(egui::Button::new(RichText::new("Start").strong()).fill(style::ACCENT))
                        .clicked()
                    {
                        self.start(ui.ctx());
                    }
                });
            });
            ui.add_space(6.0);
            egui::Grid::new("status").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                let row = |ui: &mut egui::Ui, k: &str, v: Option<&str>| {
                    ui.label(RichText::new(k).color(style::MUTED));
                    ui.label(v.unwrap_or("—"));
                    ui.end_row();
                };
                row(ui, "Other computer", status.peer.as_deref());
                if self.settings.connect.trim().is_empty() {
                    row(ui, "Listening on", status.listening.as_deref());
                } else {
                    row(ui, "Connecting to", Some(self.settings.connect.trim()));
                }
                row(ui, "Sending", status.outgoing.as_deref());
                if status.controlled {
                    row(ui, "", Some("The other computer is using this window"));
                }
                row(ui, "Encoder", status.encoder.as_deref());
                row(ui, "Receiving", status.incoming.as_deref());
                row(ui, "Decoder", status.decoder.as_deref());
                if let Some(up) = self.daemon.uptime() {
                    let s = up.as_secs();
                    row(
                        ui,
                        "Running for",
                        Some(&format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)),
                    );
                }
            });
            if let Some(p) = status.last_problem.as_deref().or(self.daemon.exit()) {
                ui.add_space(6.0);
                ui.label(RichText::new(p).color(style::WARN).small());
            }
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(style::WARN));
            }
        });
    }

    fn settings_card(&mut self, ui: &mut egui::Ui) {
        style::card(ui, |ui| {
            ui.label(RichText::new("Settings").strong());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.dial, false, "Wait for the other computer");
                ui.radio_value(&mut self.dial, true, "Connect to it");
            });
            egui::Grid::new("settings").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                if self.dial {
                    ui.label("Address");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.draft.connect)
                            .hint_text("192.168.0.19:47800"),
                    );
                } else {
                    ui.label("Port");
                    ui.add(egui::DragValue::new(&mut self.draft.listen_port).range(1024..=65535));
                }
                ui.end_row();
                ui.label("Shared secret");
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.token)
                        .password(true)
                        .hint_text("optional"),
                );
                ui.end_row();
            });
            ui.checkbox(
                &mut self.draft.remote_control,
                "Let the other computer click and type in my windows",
            );
            ui.checkbox(
                &mut self.draft.autostart_daemon,
                "Start sharing as soon as this window opens",
            );
            ui.checkbox(&mut self.draft.check_updates, "Check for updates on start");
            let mut autostart = self.autostart;
            if ui.checkbox(&mut autostart, "Start Ghost Portals when I log in").changed() {
                match install::set_autostart(&install::current_exe(), autostart) {
                    Ok(()) => self.autostart = autostart,
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
            }
            let mut draft = self.draft.clone();
            draft.connect = if self.dial { draft.connect.trim().to_owned() } else { String::new() };
            let changed = draft != self.settings;
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let apply = ui.add_enabled(changed, egui::Button::new("Apply"));
                if apply.clicked() {
                    self.settings = draft.clone();
                    if let Err(e) = self.settings.save() {
                        self.error = Some(format!("saving settings: {e:#}"));
                    }
                    if self.daemon.running() {
                        self.start(ui.ctx());
                    }
                }
                if changed && ui.button("Discard").clicked() {
                    self.draft = self.settings.clone();
                    self.dial = !self.settings.connect.is_empty();
                }
                if changed {
                    ui.label(RichText::new("Applying restarts sharing.").weak());
                }
            });
        });
    }

    fn tools_card(&mut self, ui: &mut egui::Ui) {
        style::card(ui, |ui| {
            ui.horizontal(|ui| {
                let test = self.test.lock().unwrap().clone();
                if ui
                    .add_enabled(
                        !matches!(test, TestState::Running),
                        egui::Button::new("Run self-test"),
                    )
                    .clicked()
                {
                    self.run_selftest(ui.ctx());
                }
                let checking = matches!(*self.update.lock().unwrap(), UpdateState::Checking);
                if ui.add_enabled(!checking, egui::Button::new("Check for updates")).clicked() {
                    self.check_updates(ui.ctx());
                }
                ui.toggle_value(&mut self.show_log, "Log");
            });
            match self.test.lock().unwrap().clone() {
                TestState::Running => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Encoding and decoding a test pattern…");
                    });
                }
                TestState::Done(Ok(s)) => {
                    ui.label(RichText::new(format!("Self-test passed: {s}")).color(style::GOOD));
                }
                TestState::Done(Err(e)) => {
                    ui.label(RichText::new(format!("Self-test failed: {e}")).color(style::WARN));
                }
                TestState::Idle => {}
            }
            match &*self.update.lock().unwrap() {
                UpdateState::Checking => {
                    ui.label(RichText::new("Checking for updates…").weak());
                }
                UpdateState::UpToDate => {
                    ui.label(
                        RichText::new(format!(
                            "Ghost Portals {} is up to date.",
                            update::current()
                        ))
                        .weak(),
                    );
                }
                _ => {}
            }
        });
    }

    fn log_card(&self, ui: &mut egui::Ui) {
        style::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Log").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Copy").clicked() {
                        ui.ctx().copy_text(self.daemon.log().join("\n"));
                    }
                });
            });
            egui::ScrollArea::vertical().max_height(260.0).stick_to_bottom(true).show(ui, |ui| {
                for line in self.daemon.log() {
                    let color = if line.contains(" WARN ")
                        || line.contains(" ERROR ")
                        || line.starts_with("Error")
                    {
                        style::WARN
                    } else {
                        ui.visuals().text_color()
                    };
                    ui.label(RichText::new(line).monospace().small().color(color));
                }
            });
        });
    }
}

impl eframe::App for Manager {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        // Keep uptime, process state and download progress fresh.
        if matches!(*self.update.lock().unwrap(), UpdateState::Downloading { .. }) {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.daemon.running() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Ghost Portals").size(24.0).strong());
                    ui.label(RichText::new(format!("v{}", update::current())).color(style::MUTED));
                });
                ui.add_space(8.0);
                self.update_banner(ui);
                self.status_card(ui);
                self.settings_card(ui);
                self.tools_card(ui);
                if self.show_log {
                    self.log_card(ui);
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Closing this window stops sharing. It starts again with the window.",
                    )
                    .small()
                    .color(style::MUTED),
                );
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.daemon.stop();
    }
}
