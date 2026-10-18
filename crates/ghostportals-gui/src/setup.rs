//! The Windows online installer (`GhostPortalsSetup.exe`) and uninstaller.
//!
//! The installer is this same program under another name: it fetches the
//! newest release from GitHub, verifies and unpacks it into the per-user
//! programs folder, registers it, and launches the installed manager.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use eframe::egui::{self, RichText};

use crate::install::{self, Options};
use crate::style;
use crate::update::{self, Release};

#[derive(Clone)]
enum Step {
    Looking,
    Ready(Box<Release>),
    Downloading { got: Arc<AtomicU64>, total: u64 },
    Installing,
    Done,
    Failed(String),
}

pub struct Setup {
    step: Arc<Mutex<Step>>,
    opts: Options,
    installed: Option<String>,
}

impl Setup {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        style::apply(&cc.egui_ctx);
        let step = Arc::new(Mutex::new(Step::Looking));
        let s = step.clone();
        let ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || {
            *s.lock().unwrap() = match update::latest() {
                Ok(r) if r.asset(update::WINDOWS_ASSET).is_some() => Step::Ready(Box::new(r)),
                Ok(r) => Step::Failed(format!("{} has no Windows download yet", r.tag_name)),
                Err(e) => Step::Failed(format!("{e:#}")),
            };
            ctx.request_repaint();
        });
        Self {
            step,
            opts: Options { desktop_shortcut: true, start_on_login: true },
            installed: install::installed_version(&install::install_dir()),
        }
    }

    fn begin(&self, ctx: &egui::Context, release: Release) {
        let asset = release.asset(update::WINDOWS_ASSET).cloned().expect("checked when found");
        let got = Arc::new(AtomicU64::new(0));
        *self.step.lock().unwrap() = Step::Downloading { got: got.clone(), total: asset.size };
        let step = self.step.clone();
        let opts = self.opts;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let version = release.tag_name.trim_start_matches('v').to_owned();
            let result = (|| -> Result<()> {
                let zip = std::env::temp_dir().join(format!("ghostportals-{version}.zip"));
                update::download(&asset, &zip, &got)?;
                *step.lock().unwrap() = Step::Installing;
                ctx.request_repaint();
                install::install(&zip, &install::install_dir(), &version, opts)?;
                let _ = std::fs::remove_file(&zip);
                Ok(())
            })();
            *step.lock().unwrap() = match result {
                Ok(()) => Step::Done,
                Err(e) => Step::Failed(format!("{e:#}")),
            };
            ctx.request_repaint();
        });
    }
}

impl eframe::App for Setup {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        let step = self.step.lock().unwrap().clone();
        if matches!(step, Step::Downloading { .. } | Step::Installing) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(6.0);
            ui.label(RichText::new("Ghost Portals").size(26.0).strong());
            ui.label(
                RichText::new("Drag windows between your Windows and Linux computers.")
                    .color(style::MUTED),
            );
            ui.add_space(12.0);
            style::card(ui, |ui| match &step {
                Step::Looking => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Looking up the newest version…");
                    });
                }
                Step::Ready(release) => {
                    ui.label(RichText::new(format!("Version {}", release.tag_name)).strong());
                    if let Some(v) = &self.installed {
                        ui.label(RichText::new(format!("Installed now: {v}")).color(style::MUTED));
                    }
                    ui.label(
                        RichText::new(format!("Installs to {}", install::install_dir().display()))
                            .small()
                            .color(style::MUTED),
                    );
                    ui.add_space(6.0);
                    ui.checkbox(&mut self.opts.desktop_shortcut, "Put a shortcut on the desktop");
                    ui.checkbox(&mut self.opts.start_on_login, "Start Ghost Portals when I log in");
                    ui.add_space(8.0);
                    let label = if self.installed.is_some() { "Update" } else { "Install" };
                    let button = egui::Button::new(RichText::new(label).strong().size(16.0))
                        .fill(style::ACCENT)
                        .min_size(egui::vec2(140.0, 34.0));
                    if ui.add(button).clicked() {
                        self.begin(ui.ctx(), *release.clone());
                    }
                }
                Step::Downloading { got, total } => {
                    let got = got.load(Ordering::Relaxed);
                    ui.label("Downloading…");
                    ui.add(
                        egui::ProgressBar::new(got as f32 / (*total).max(1) as f32).text(format!(
                            "{:.1} / {:.1} MB",
                            got as f64 / 1e6,
                            *total as f64 / 1e6
                        )),
                    );
                }
                Step::Installing => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Installing…");
                    });
                }
                Step::Done => {
                    ui.label(
                        RichText::new("Ghost Portals is installed.").strong().color(style::GOOD),
                    );
                    ui.label("Find it in the Start menu. Remove it any time from Settings → Apps.");
                    ui.add_space(8.0);
                    let open = egui::Button::new(RichText::new("Open Ghost Portals").strong())
                        .fill(style::ACCENT)
                        .min_size(egui::vec2(160.0, 34.0));
                    if ui.add(open).clicked() {
                        let exe = install::install_dir().join(install::MANAGER_EXE);
                        let _ = std::process::Command::new(exe).spawn();
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                Step::Failed(e) => {
                    ui.label(RichText::new("Something went wrong").strong().color(style::WARN));
                    ui.label(e);
                    ui.add_space(6.0);
                    if ui.button("Open the downloads page").clicked() {
                        let _ = open::that(update::RELEASES_PAGE);
                    }
                }
            });
        });
    }
}

/// `--silent [--from-zip PATH] [--dir DIR]`: installs without a window, for
/// scripts and CI. Without `--from-zip` it downloads the newest release.
pub fn silent(from_zip: Option<PathBuf>, dir: Option<PathBuf>) -> Result<()> {
    let dir = dir.unwrap_or_else(install::install_dir);
    let opts = Options { desktop_shortcut: false, start_on_login: false };
    let (zip, version, temp) = match from_zip {
        Some(zip) => (zip, update::current().to_string(), false),
        None => {
            let release = update::latest()?;
            let asset = release
                .asset(update::WINDOWS_ASSET)
                .ok_or_else(|| anyhow!("{} has no Windows download", release.tag_name))?;
            let zip = std::env::temp_dir().join(format!("{}-{}", release.tag_name, asset.name));
            update::download(asset, &zip, &Arc::new(AtomicU64::new(0)))?;
            (zip, release.tag_name.trim_start_matches('v').to_owned(), true)
        }
    };
    install::install(&zip, &dir, &version, opts).context("installing")?;
    if temp {
        let _ = std::fs::remove_file(&zip);
    }
    println!("installed Ghost Portals {version} to {}", dir.display());
    Ok(())
}

/// Confirms, then removes the installation.
pub struct Uninstall {
    done: Option<Result<(), String>>,
}

impl Uninstall {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        style::apply(&cc.egui_ctx);
        Self { done: None }
    }
}

impl eframe::App for Uninstall {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.label(RichText::new("Remove Ghost Portals?").size(22.0).strong());
            ui.add_space(8.0);
            match &self.done {
                None => {
                    ui.label("This removes the program, its shortcuts and its settings.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new("Remove").fill(style::WARN)).clicked() {
                            self.done = Some(install::uninstall().map_err(|e| format!("{e:#}")));
                        }
                        if ui.button("Cancel").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                Some(Ok(())) => {
                    ui.label(RichText::new("Ghost Portals has been removed.").color(style::GOOD));
                    if ui.button("Close").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e).color(style::WARN));
                }
            }
        });
    }
}
