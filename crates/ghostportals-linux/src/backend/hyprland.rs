//! Hyprland backend, over its IPC sockets.
//!
//! `.socket2.sock` announces windows opening and closing, but nothing fires
//! while a window is being dragged, so the active window is also polled.
//! Hyprland has no "being dragged" flag in IPC; a window whose position keeps
//! changing counts as moving.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ghostportals_core::Rect;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::{BackendEvent, WindowInfo};

const POLL: Duration = Duration::from_millis(16);
const MOVING_GRACE: Duration = Duration::from_millis(150);

pub struct Hyprland {
    dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Client {
    address: String,
    #[serde(default)]
    mapped: bool,
    #[serde(default)]
    hidden: bool,
    at: [i32; 2],
    size: [i32; 2],
    #[serde(default)]
    class: String,
    #[serde(default)]
    title: String,
}

#[derive(Debug, Deserialize)]
struct Monitor {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    #[serde(default = "one")]
    scale: f64,
    #[serde(default)]
    transform: i32,
}

fn one() -> f64 {
    1.0
}

impl Client {
    fn info(&self, active: bool, moving: bool) -> WindowInfo {
        WindowInfo {
            id: self.address.clone(),
            title: self.title.clone(),
            app_id: self.class.clone(),
            rect: Rect::new(self.at[0], self.at[1], self.size[0], self.size[1]),
            active,
            moving,
        }
    }
}

impl Hyprland {
    pub async fn connect(events: mpsc::Sender<BackendEvent>) -> Result<Self> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .context("HYPRLAND_INSTANCE_SIGNATURE is not set (not a Hyprland session?)")?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let dir = [runtime.map(|r| r.join("hypr")), Some(PathBuf::from("/tmp/hypr"))]
            .into_iter()
            .flatten()
            .map(|d| d.join(&sig))
            .find(|d| d.join(".socket.sock").exists())
            .context("cannot find Hyprland's IPC socket")?;

        let this = Self { dir };
        this.send_desktop(&events).await?;
        for c in this.clients().await? {
            if c.mapped && !c.hidden {
                let _ = events.send(BackendEvent::Window(c.info(false, false))).await;
            }
        }

        let events_sock = UnixStream::connect(this.dir.join(".socket2.sock"))
            .await
            .context("connecting to Hyprland's event socket")?;
        tokio::spawn(Self { dir: this.dir.clone() }.listen(events_sock, events.clone()));
        tokio::spawn(Self { dir: this.dir.clone() }.poll_active(events));
        Ok(this)
    }

    pub async fn place_window(&self, id: &str, r: Rect) -> Result<()> {
        // Only floating windows can be placed freely.
        let reply = self
            .request(&format!(
                "[[BATCH]]dispatch setfloating address:{id};\
                 dispatch resizewindowpixel exact {} {},address:{id};\
                 dispatch movewindowpixel exact {} {},address:{id}",
                r.width, r.height, r.x, r.y
            ))
            .await?;
        // A batch answers "ok" per command, with or without separators.
        if !reply.replace("ok", "").trim().is_empty() {
            bail!("Hyprland refused move: {reply}");
        }
        Ok(())
    }

    async fn request(&self, cmd: &str) -> Result<String> {
        let mut s = UnixStream::connect(self.dir.join(".socket.sock")).await?;
        s.write_all(cmd.as_bytes()).await?;
        let mut out = String::new();
        s.read_to_string(&mut out).await?;
        Ok(out)
    }

    async fn clients(&self) -> Result<Vec<Client>> {
        Ok(serde_json::from_str(&self.request("j/clients").await?)?)
    }

    async fn active(&self) -> Result<Option<Client>> {
        let reply = self.request("j/activewindow").await?;
        // `{}` when nothing is focused.
        Ok(serde_json::from_str(&reply).ok())
    }

    async fn send_desktop(&self, events: &mpsc::Sender<BackendEvent>) -> Result<()> {
        let monitors: Vec<Monitor> = serde_json::from_str(&self.request("j/monitors").await?)?;
        if let Some(bounds) = desktop_bounds(&monitors) {
            let _ = events.send(BackendEvent::Desktop(bounds)).await;
        }
        Ok(())
    }

    async fn listen(self, sock: UnixStream, events: mpsc::Sender<BackendEvent>) {
        let mut lines = BufReader::new(sock).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => {
                    warn!("Hyprland event socket closed");
                    return;
                }
                Err(e) => {
                    warn!("Hyprland event socket: {e}");
                    return;
                }
            };
            let Some((event, data)) = line.split_once(">>") else { continue };
            let result = match event {
                "closewindow" => events
                    .send(BackendEvent::Removed(format!("0x{data}")))
                    .await
                    .map_err(anyhow::Error::from),
                "openwindow" | "windowtitlev2" | "movewindowv2" | "changefloatingmode" => {
                    self.resync(&events).await
                }
                "monitoradded" | "monitorremoved" | "monitoraddedv2" | "monitorremovedv2" => {
                    self.send_desktop(&events).await
                }
                _ => Ok(()),
            };
            if let Err(e) = result {
                debug!("handling Hyprland event {event}: {e}");
            }
        }
    }

    async fn resync(&self, events: &mpsc::Sender<BackendEvent>) -> Result<()> {
        for c in self.clients().await? {
            if c.mapped && !c.hidden {
                events.send(BackendEvent::Window(c.info(false, false))).await?;
            }
        }
        Ok(())
    }

    async fn poll_active(self, events: mpsc::Sender<BackendEvent>) {
        let mut ticker = tokio::time::interval(POLL);
        let mut last: HashMap<String, (Rect, Instant)> = HashMap::new();
        loop {
            ticker.tick().await;
            let client = match self.active().await {
                Ok(Some(c)) => c,
                Ok(None) => continue,
                Err(e) => {
                    debug!("polling Hyprland: {e}");
                    continue;
                }
            };
            let now = Instant::now();
            let rect = Rect::new(client.at[0], client.at[1], client.size[0], client.size[1]);
            let changed_at = match last.get(&client.address) {
                Some((r, t)) if *r == rect => *t,
                Some(_) => now,
                None => now - MOVING_GRACE,
            };
            last.retain(|k, _| *k == client.address);
            last.insert(client.address.clone(), (rect, changed_at));
            let moving = now.duration_since(changed_at) < MOVING_GRACE;
            if events.send(BackendEvent::Window(client.info(true, moving))).await.is_err() {
                return;
            }
        }
    }
}

/// Union of all monitors in logical (scaled) layout coordinates.
fn desktop_bounds(monitors: &[Monitor]) -> Option<Rect> {
    monitors
        .iter()
        .map(|m| {
            let (w, h) =
                if m.transform % 2 == 1 { (m.height, m.width) } else { (m.width, m.height) };
            let s = if m.scale > 0.0 { m.scale } else { 1.0 };
            Rect::new(m.x, m.y, (w as f64 / s).round() as i32, (h as f64 / s).round() as i32)
        })
        .reduce(|a, b| {
            let x = a.x.min(b.x);
            let y = a.y.min(b.y);
            Rect::new(x, y, a.right().max(b.right()) - x, (a.y + a.height).max(b.y + b.height) - y)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipc_json() {
        let clients: Vec<Client> = serde_json::from_str(
            r#"[{"address":"0x55d1","mapped":true,"hidden":false,"at":[-20,40],"size":[800,600],
                 "workspace":{"id":1,"name":"1"},"floating":true,"class":"foot","title":"~"}]"#,
        )
        .unwrap();
        assert_eq!(clients[0].info(true, false).rect, Rect::new(-20, 40, 800, 600));

        let monitors: Vec<Monitor> = serde_json::from_str(
            r#"[{"id":0,"x":0,"y":0,"width":3840,"height":2160,"scale":2.0,"transform":0},
                {"id":1,"x":1920,"y":0,"width":1080,"height":1920,"scale":1.0,"transform":1}]"#,
        )
        .unwrap();
        assert_eq!(desktop_bounds(&monitors), Some(Rect::new(0, 0, 3840, 1080)));
    }
}
