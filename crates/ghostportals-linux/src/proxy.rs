//! The proxy window: a borderless GTK window that shows a window streamed from
//! the other machine.
//!
//! GTK runs on its own thread. The daemon talks to it through [`Proxy`],
//! which never blocks: frames go through a one-slot mailbox (only the newest
//! frame matters), commands through an unbounded channel. The compositor
//! backend finds the window by [`APP_ID`] and places it like any other.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ghostportals_media::RgbaFrame;
use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use tracing::{info, warn};

/// The Wayland app id (and KWin resource class) of every proxy window.
pub const APP_ID: &str = "io.ghostportals.proxy";

/// Height of the strip along the top that drags the proxy, like a title bar.
/// The streamed window's own title bar is drawn there.
const DRAG_STRIP: f64 = 32.0;

enum Command {
    Show { width: i32, height: i32, title: String },
    Hide,
}

#[derive(Clone)]
pub struct Proxy {
    commands: async_channel::Sender<Command>,
    frame: Arc<Mutex<Option<RgbaFrame>>>,
    wake: async_channel::Sender<()>,
}

impl Proxy {
    /// Starts the GTK thread. Fails if GTK cannot open the display.
    pub fn spawn() -> Result<Self> {
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (wake_tx, wake_rx) = async_channel::bounded(1);
        let frame: Arc<Mutex<Option<RgbaFrame>>> = Arc::default();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let slot = frame.clone();
        std::thread::Builder::new()
            .name("gtk".into())
            .spawn(move || {
                // On Wayland GTK uses the program name as the app id, which
                // is how the compositor backend recognises proxies.
                glib::set_prgname(Some(APP_ID));
                if let Err(e) = gtk::init() {
                    let _ = ready_tx.send(Err(anyhow::anyhow!("GTK: {e}")));
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                run(cmd_rx, wake_rx, slot);
            })
            .context("starting the GTK thread")?;
        ready_rx.recv().context("GTK thread died")??;
        Ok(Self { commands: cmd_tx, frame, wake: wake_tx })
    }

    pub fn show(&self, width: i32, height: i32, title: &str) {
        let _ = self.commands.try_send(Command::Show { width, height, title: title.to_owned() });
    }

    pub fn hide(&self) {
        let _ = self.commands.try_send(Command::Hide);
    }

    /// Replaces the pending frame. Callable from any thread.
    pub fn frame(&self, frame: RgbaFrame) {
        *self.frame.lock().unwrap() = Some(frame);
        let _ = self.wake.try_send(());
    }
}

fn run(
    commands: async_channel::Receiver<Command>,
    wake: async_channel::Receiver<()>,
    slot: Arc<Mutex<Option<RgbaFrame>>>,
) {
    let window = gtk::Window::builder().decorated(false).resizable(true).build();
    window.set_size_request(1, 1);
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Fill)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    window.set_child(Some(&picture));

    // Dragging the top strip moves the proxy through the compositor; the
    // daemon sees the move and drags the real window along.
    let press = gtk::GestureClick::builder().button(1).build();
    let w = window.clone();
    press.connect_pressed(move |gesture, _, x, y| {
        if y > DRAG_STRIP {
            return;
        }
        let (Some(device), Some(toplevel)) = (
            gesture.current_event_device(),
            w.surface().and_then(|s| s.downcast::<gdk::Toplevel>().ok()),
        ) else {
            return;
        };
        toplevel.begin_move(&device, 1, x, y, gesture.current_event_time());
    });
    picture.add_controller(press);
    // Closing a proxy means nothing; the stream decides when it goes away.
    window.connect_close_request(|w| {
        w.set_visible(false);
        glib::Propagation::Stop
    });

    let ctx = glib::MainContext::default();
    let w = window.clone();
    ctx.spawn_local(async move {
        while let Ok(cmd) = commands.recv().await {
            match cmd {
                Command::Show { width, height, title } => {
                    w.set_title(Some(&format!("{title} (Ghost Portal)")));
                    w.set_default_size(width, height);
                    if !w.is_visible() {
                        info!("showing proxy for \"{title}\" at {width}x{height}");
                        w.present();
                    }
                }
                Command::Hide => w.set_visible(false),
            }
        }
    });
    ctx.spawn_local(async move {
        while wake.recv().await.is_ok() {
            let Some(f) = slot.lock().unwrap().take() else { continue };
            let bytes = glib::Bytes::from_owned(f.data);
            let texture = gdk::MemoryTexture::new(
                f.width,
                f.height,
                gdk::MemoryFormat::R8g8b8a8,
                &bytes,
                f.stride,
            );
            picture.set_paintable(Some(&texture));
        }
    });
    let main_loop = glib::MainLoop::new(Some(&ctx), false);
    main_loop.run();
    warn!("GTK main loop ended");
}
