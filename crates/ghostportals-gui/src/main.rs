//! Ghost Portals manager: a window for running the daemon without a
//! terminal. On Windows the same program is also the online installer
//! (when named `GhostPortalsSetup.exe` or run with `--setup`) and the
//! uninstaller (`--uninstall`).

// No console window behind the GUI on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod daemon;
// Downloading and installing are Windows-only; Linux only checks.
#[cfg_attr(not(windows), allow(dead_code))]
mod install;
mod settings;
#[cfg(windows)]
mod setup;
mod style;
#[cfg_attr(not(windows), allow(dead_code))]
mod update;

use std::path::PathBuf;

use eframe::egui;

struct Args {
    minimized: bool,
    setup: bool,
    uninstall: bool,
    silent: bool,
    from_zip: Option<PathBuf>,
    dir: Option<PathBuf>,
}

fn args() -> Args {
    let mut a = Args {
        minimized: false,
        setup: false,
        uninstall: false,
        silent: false,
        from_zip: None,
        dir: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--minimized" => a.minimized = true,
            "--setup" => a.setup = true,
            "--uninstall" => a.uninstall = true,
            "--silent" => a.silent = true,
            "--from-zip" => a.from_zip = it.next().map(PathBuf::from),
            "--dir" => a.dir = it.next().map(PathBuf::from),
            "--version" => {
                println!("ghostportals {}", update::current());
                std::process::exit(0);
            }
            _ => {}
        }
    }
    // Downloaded as GhostPortalsSetup.exe, it installs.
    let stem = install::current_exe()
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    a.setup |= stem.contains("setup");
    a
}

fn options(title: &str, size: [f32; 2]) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_app_id("io.ghostportals.manager")
            .with_inner_size(size)
            .with_min_inner_size([420.0, 300.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon.png"))
                    .expect("the bundled icon is a valid PNG"),
            ),
        ..Default::default()
    }
}

fn main() -> anyhow::Result<()> {
    let args = args();

    #[cfg(windows)]
    {
        if args.uninstall {
            if args.silent {
                return install::uninstall();
            }
            return run(
                "Remove Ghost Portals",
                [440.0, 220.0],
                Box::new(|cc| Ok(Box::new(setup::Uninstall::new(cc)))),
            );
        }
        if args.setup {
            if args.silent {
                return setup::silent(args.from_zip, args.dir);
            }
            return run(
                "Install Ghost Portals",
                [480.0, 360.0],
                Box::new(|cc| Ok(Box::new(setup::Setup::new(cc)))),
            );
        }
        install::clean_old_files();
    }
    #[cfg(not(windows))]
    if args.setup || args.uninstall || args.silent || args.from_zip.is_some() || args.dir.is_some()
    {
        anyhow::bail!(
            "installing is only for Windows; on Linux build or package the daemon instead"
        );
    }

    let minimized = args.minimized;
    run(
        "Ghost Portals",
        [520.0, 720.0],
        Box::new(move |cc| Ok(Box::new(app::Manager::new(cc, minimized)))),
    )
}

fn run(title: &str, size: [f32; 2], app: eframe::AppCreator<'_>) -> anyhow::Result<()> {
    eframe::run_native(title, options(title, size), app).map_err(|e| anyhow::anyhow!("{e}"))
}
