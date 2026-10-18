//! The proxy window: a borderless GTK window that shows a window streamed from
//! the other machine.
//!
//! GTK runs on its own thread. The daemon talks to it through [`Proxy`],
//! which never blocks: frames go through a one-slot mailbox (only the newest
//! frame matters), commands through an unbounded channel. The compositor
//! backend finds the window by [`APP_ID`] and places it like any other.
//!
//! Pointer, wheel and key input on the proxy comes back as
//! [`InputEvent`]s for the daemon to forward to the real window.

use std::sync::{Arc, Mutex};

use std::cell::Cell;
use std::rc::Rc;

use anyhow::{Context, Result};
use ghostportals_core::{InputEvent, PointerButton};
use ghostportals_media::RgbaFrame;
use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

/// The Wayland app id (and KWin resource class) of every proxy window.
pub const APP_ID: &str = "io.ghostportals.proxy";

/// Height of the strip along the top that drags the proxy, like a title bar.
/// The streamed window's own title bar is drawn there. A click there without
/// dragging still goes to the real window, for its title bar buttons.
const DRAG_STRIP: f64 = 32.0;

/// How far the pointer must move with the button down in the strip before it
/// counts as a drag rather than a click.
const DRAG_THRESHOLD: f64 = 4.0;

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
    /// Starts the GTK thread. Fails if GTK cannot open the display. Input on
    /// the proxy is sent to `input`.
    pub fn spawn(input: UnboundedSender<InputEvent>) -> Result<Self> {
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
                run(cmd_rx, wake_rx, slot, input);
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

fn button(n: u32) -> Option<PointerButton> {
    Some(match n {
        1 => PointerButton::Left,
        2 => PointerButton::Middle,
        3 => PointerButton::Right,
        8 => PointerButton::Back,
        9 => PointerButton::Forward,
        _ => return None,
    })
}

fn run(
    commands: async_channel::Receiver<Command>,
    wake: async_channel::Receiver<()>,
    slot: Arc<Mutex<Option<RgbaFrame>>>,
    input: UnboundedSender<InputEvent>,
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

    // A press in the top strip waits to see whether it becomes a drag.
    let strip_press: Rc<Cell<Option<(f64, f64)>>> = Rc::default();

    let motion = gtk::EventControllerMotion::new();
    let (tx, pending, w) = (input.clone(), strip_press.clone(), window.clone());
    motion.connect_motion(move |m, x, y| {
        if let Some((px, py)) = pending.get() {
            if (x - px).hypot(y - py) < DRAG_THRESHOLD {
                return;
            }
            // Dragging the strip moves the proxy through the compositor; the
            // daemon sees the move and drags the real window along.
            pending.set(None);
            let (Some(device), Some(toplevel)) = (
                m.current_event_device(),
                w.surface().and_then(|s| s.downcast::<gdk::Toplevel>().ok()),
            ) else {
                return;
            };
            toplevel.begin_move(&device, 1, px, py, m.current_event_time());
            return;
        }
        let _ = tx.send(InputEvent::PointerMove { x, y });
    });
    let tx = input.clone();
    motion.connect_leave(move |_| {
        let _ = tx.send(InputEvent::Leave);
    });
    picture.add_controller(motion);

    let click = gtk::GestureClick::builder().button(0).build();
    let (tx, pending) = (input.clone(), strip_press.clone());
    click.connect_pressed(move |g, _, x, y| {
        let n = g.current_button();
        if n == 1 && y <= DRAG_STRIP {
            pending.set(Some((x, y)));
            return;
        }
        if let Some(button) = button(n) {
            let _ = tx.send(InputEvent::PointerMove { x, y });
            let _ = tx.send(InputEvent::Button { button, pressed: true });
        }
    });
    let (tx, pending) = (input.clone(), strip_press.clone());
    click.connect_released(move |g, _, x, y| {
        let n = g.current_button();
        // A strip press that never became a drag is a click after all.
        if n == 1 && pending.take().is_some() {
            let _ = tx.send(InputEvent::PointerMove { x, y });
            let _ = tx.send(InputEvent::Button { button: PointerButton::Left, pressed: true });
        }
        if let Some(button) = button(n) {
            let _ = tx.send(InputEvent::Button { button, pressed: false });
        }
    });
    picture.add_controller(click);

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let tx = input.clone();
    scroll.connect_scroll(move |c, dx, dy| {
        // Touchpads report pixels; wheels report notches.
        let scale = if c.unit() == gdk::ScrollUnit::Surface { 0.1 } else { 1.0 };
        let _ = tx.send(InputEvent::Scroll { dx: dx * scale, dy: dy * scale });
        glib::Propagation::Stop
    });
    picture.add_controller(scroll);

    let keys = gtk::EventControllerKey::new();
    let tx = input.clone();
    keys.connect_key_pressed(move |_, _, keycode, _| {
        // GTK hands out XKB keycodes, which are evdev codes plus 8.
        let _ = tx.send(InputEvent::Key { code: keycode.saturating_sub(8), pressed: true });
        glib::Propagation::Stop
    });
    let tx = input;
    keys.connect_key_released(move |_, _, keycode, _| {
        let _ = tx.send(InputEvent::Key { code: keycode.saturating_sub(8), pressed: false });
    });
    window.add_controller(keys);

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
