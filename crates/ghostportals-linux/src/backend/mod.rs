//! Compositor backends.
//!
//! Wayland gives ordinary clients no way to see or move other clients'
//! windows, and the toplevel-listing protocols that exist
//! (ext-foreign-toplevel-list, wlr-foreign-toplevel-management) carry no
//! geometry. So each compositor is reached through its own privileged channel:
//! a KWin script, Hyprland's IPC socket, or a GNOME Shell extension. All of
//! them report the same [`WindowInfo`] stream and take the same commands.

mod gnome;
mod hyprland;
mod kwin;

use std::fmt;

use anyhow::{Result, bail};
use ghostportals_core::Rect;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub id: String,
    pub title: String,
    pub app_id: String,
    /// Frame geometry in global desktop coordinates.
    pub rect: Rect,
    pub active: bool,
    /// Being dragged by the user right now.
    pub moving: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BackendEvent {
    Window(WindowInfo),
    Removed(String),
    /// Bounds of the whole desktop changed (outputs added, moved, resized).
    Desktop(Rect),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Kind {
    Kwin,
    Hyprland,
    Gnome,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Kwin => "kwin",
            Kind::Hyprland => "hyprland",
            Kind::Gnome => "gnome",
        })
    }
}

impl Kind {
    /// Picks the backend for the running session from the environment.
    pub fn detect() -> Result<Self> {
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            return Ok(Kind::Hyprland);
        }
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
        if desktop.split(':').any(|d| d == "kde") {
            return Ok(Kind::Kwin);
        }
        if desktop.split(':').any(|d| d == "gnome" || d == "ubuntu" || d == "unity") {
            return Ok(Kind::Gnome);
        }
        bail!("unsupported desktop {desktop:?}; pass --backend kwin|hyprland|gnome to force one")
    }
}

pub enum Backend {
    Kwin(kwin::Kwin),
    Hyprland(hyprland::Hyprland),
    Gnome(gnome::Gnome),
}

impl Backend {
    /// Connects to the compositor. Window events start flowing into the
    /// returned receiver straight away, beginning with every existing window.
    pub async fn connect(kind: Kind) -> Result<(Self, mpsc::Receiver<BackendEvent>)> {
        let (tx, rx) = mpsc::channel(256);
        let backend = match kind {
            Kind::Kwin => Backend::Kwin(kwin::Kwin::connect(tx).await?),
            Kind::Hyprland => Backend::Hyprland(hyprland::Hyprland::connect(tx).await?),
            Kind::Gnome => Backend::Gnome(gnome::Gnome::connect(tx).await?),
        };
        Ok((backend, rx))
    }

    /// Sets a window's frame to `rect` in global desktop coordinates,
    /// un-maximizing or un-tiling it first. Compositors may refuse or clamp
    /// positions that are mostly off-screen, and clients may round sizes.
    pub async fn place_window(&self, id: &str, rect: Rect) -> Result<()> {
        match self {
            Backend::Kwin(b) => b.place_window(id, rect).await,
            Backend::Hyprland(b) => b.place_window(id, rect).await,
            Backend::Gnome(b) => b.place_window(id, rect).await,
        }
    }

    /// Undoes whatever `connect` installed in the compositor.
    pub async fn shutdown(self) -> Result<()> {
        match self {
            Backend::Kwin(b) => b.shutdown().await,
            Backend::Hyprland(_) | Backend::Gnome(_) => Ok(()),
        }
    }
}
