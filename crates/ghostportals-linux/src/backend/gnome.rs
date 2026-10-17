//! GNOME Shell (Mutter) backend.
//!
//! Mutter only exposes window geometry to Shell extensions, so this talks to
//! the companion extension in `extensions/gnome`, which serves
//! `io.ghostportals.Shell` on the session bus.

use anyhow::{Context, Result};
use futures::StreamExt;
use ghostportals_core::Rect;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::warn;
use zbus::{Connection, proxy};

use super::{BackendEvent, WindowInfo};

#[proxy(
    interface = "io.ghostportals.Shell",
    default_service = "io.ghostportals.Shell",
    default_path = "/io/ghostportals/Shell"
)]
trait Shell {
    fn list_windows(&self) -> zbus::Result<String>;
    fn desktop_bounds(&self) -> zbus::Result<String>;
    fn move_window(&self, id: &str, x: i32, y: i32) -> zbus::Result<()>;
    #[zbus(signal)]
    fn window_changed(&self, payload: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn window_removed(&self, id: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn desktop_changed(&self, payload: String) -> zbus::Result<()>;
}

#[derive(Deserialize)]
struct Window {
    id: String,
    title: String,
    app: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    active: bool,
    moving: bool,
}

impl From<Window> for WindowInfo {
    fn from(w: Window) -> Self {
        WindowInfo {
            id: w.id,
            title: w.title,
            app_id: w.app,
            rect: Rect::new(w.x, w.y, w.width, w.height),
            active: w.active,
            moving: w.moving,
        }
    }
}

pub struct Gnome {
    shell: ShellProxy<'static>,
}

impl Gnome {
    pub async fn connect(events: mpsc::Sender<BackendEvent>) -> Result<Self> {
        let conn = Connection::session().await?;
        let shell = ShellProxy::new(&conn).await?;

        // Subscribe before listing so nothing slips between the two.
        let mut changed = shell.receive_window_changed().await?;
        let mut removed = shell.receive_window_removed().await?;
        let mut desktop = shell.receive_desktop_changed().await?;

        let bounds: Rect = serde_json::from_str(&shell.desktop_bounds().await.context(
            "the Ghost Portals GNOME Shell extension is not running \
             (install extensions/gnome and enable it with gnome-extensions)",
        )?)?;
        let _ = events.send(BackendEvent::Desktop(bounds)).await;
        let windows: Vec<Window> = serde_json::from_str(&shell.list_windows().await?)?;
        for w in windows {
            let _ = events.send(BackendEvent::Window(w.into())).await;
        }

        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    Some(s) = changed.next() => s.args().ok().and_then(|a| {
                        serde_json::from_str::<Window>(&a.payload)
                            .map_err(|e| warn!("bad window from extension: {e}"))
                            .ok()
                            .map(|w| BackendEvent::Window(w.into()))
                    }),
                    Some(s) = removed.next() => s.args().ok().map(|a| BackendEvent::Removed(a.id)),
                    Some(s) = desktop.next() => s.args().ok().and_then(|a| {
                        serde_json::from_str::<Rect>(&a.payload).ok().map(BackendEvent::Desktop)
                    }),
                    else => {
                        warn!("lost the GNOME Shell extension's signals");
                        return;
                    }
                };
                if let Some(event) = event
                    && events.send(event).await.is_err()
                {
                    return;
                }
            }
        });
        Ok(Self { shell })
    }

    pub async fn move_window(&self, id: &str, x: i32, y: i32) -> Result<()> {
        Ok(self.shell.move_window(id, x, y).await?)
    }
}
