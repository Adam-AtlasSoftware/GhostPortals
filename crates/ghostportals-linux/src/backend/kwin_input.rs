//! Input injection on KWin through `org_kde_kwin_fake_input`, granted by the
//! same `.desktop` file as window capture.
//!
//! The protocol drives the seat's real pointer and keyboard, so events land
//! wherever the pointer is and whichever window has focus. The daemon makes
//! sure that is the controlled window first.

use std::sync::mpsc;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, warn};
use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry};

#[allow(dead_code, non_camel_case_types, non_upper_case_globals, unused_imports, clippy::all)]
mod proto {
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/fake-input.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/fake-input.xml");
}

use proto::org_kde_kwin_fake_input as fake;

/// `keyboard_key` arrived in version 4.
const MIN_VERSION: u32 = 4;

pub enum Event {
    /// Global compositor coordinates.
    MoveTo { x: f64, y: f64 },
    /// Linux evdev button code (`BTN_LEFT` = 0x110).
    Button { code: u32, pressed: bool },
    /// Axis 0 is vertical, 1 horizontal; value in scroll units (15 per notch).
    Axis { axis: u32, value: f64 },
    /// Linux evdev key code.
    Key { code: u32, pressed: bool },
}

/// A connection to KWin's fake input device. Events are sent from its own
/// thread, so sending never blocks the daemon.
pub struct Injector {
    tx: mpsc::Sender<Event>,
}

#[derive(Default)]
struct State {
    fake: Option<fake::OrgKdeKwinFakeInput>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event
            && interface == "org_kde_kwin_fake_input"
            && version >= MIN_VERSION
        {
            state.fake = Some(registry.bind(name, version.min(5), qh, ()));
        }
    }
}

impl Dispatch<fake::OrgKdeKwinFakeInput, ()> for State {
    fn event(
        _: &mut Self,
        _: &fake::OrgKdeKwinFakeInput,
        _: fake::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Injector {
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland display")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state)?;
        let Some(fake) = state.fake.take() else {
            bail!(
                "KWin does not offer input injection to this program (fake input v{MIN_VERSION}+)"
            );
        };
        fake.authenticate(
            "Ghost Portals".into(),
            "Replays clicks and keys from the other computer on its windows".into(),
        );
        conn.flush()?;
        let (tx, rx) = mpsc::channel::<Event>();
        std::thread::Builder::new()
            .name("kwin-input".into())
            .spawn(move || {
                for event in rx {
                    match event {
                        Event::MoveTo { x, y } => fake.pointer_motion_absolute(x, y),
                        Event::Button { code, pressed } => fake.button(code, pressed as u32),
                        Event::Axis { axis, value } => fake.axis(axis, value),
                        Event::Key { code, pressed } => fake.keyboard_key(code, pressed as u32),
                    }
                    if let Err(e) = conn.flush() {
                        warn!("KWin input connection lost: {e}");
                        return;
                    }
                }
                debug!("input injector closed");
            })
            .map_err(|e| anyhow!("starting the input thread: {e}"))?;
        Ok(Self { tx })
    }

    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }
}
