//! Replaying input from a proxy on the real window.
//!
//! Mouse input is posted straight to the window under the point instead of
//! being injected with `SendInput`: that works while the window hangs off the
//! screen (where the real cursor cannot go) and leaves the cursor, which an
//! input-sharing program may be watching, alone. Keys do go through
//! `SendInput`, as scan codes, after the window is brought to the foreground:
//! posted key messages would not update the keyboard state apps read for
//! Shift and friends.

use ghostportals_core::keys::scancode_from_evdev;
use ghostportals_core::{InputEvent, PointerButton};
use tracing::debug;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CWP_SKIPDISABLED, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT,
    ChildWindowFromPointEx, GetClientRect, GetForegroundWindow, GetWindowThreadProcessId, HTCLIENT,
    HTCLOSE, HTMAXBUTTON, HTMINBUTTON, IsZoomed, PostMessageW, SC_CLOSE, SC_MAXIMIZE, SC_MINIMIZE,
    SC_RESTORE, SMTO_ABORTIFHUNG, SendMessageTimeoutW, SetForegroundWindow, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCHITTEST, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSCOMMAND, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use crate::win32::Window;

// Button flags in mouse messages' wParam (MK_*), fixed by Win32.
const MK_LBUTTON: u32 = 0x01;
const MK_RBUTTON: u32 = 0x02;
const MK_MBUTTON: u32 = 0x10;
const MK_XBUTTON1: u32 = 0x20;
const MK_XBUTTON2: u32 = 0x40;

/// Replays proxy input on one window. Keeps the pointer position and held
/// buttons between events, as a real mouse would.
#[derive(Debug, Default)]
pub struct Injector {
    /// Frame-relative pointer position.
    x: f64,
    y: f64,
    buttons: u32,
    /// Title-bar button pressed on the last button-down, acted on at button-up.
    nc_hit: Option<u32>,
    focused: Option<Window>,
}

fn pack(x: i32, y: i32) -> isize {
    ((y as u16 as u32) << 16 | x as u16 as u32) as isize
}

fn post(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) {
    let _ = unsafe { PostMessageW(Some(hwnd), msg, WPARAM(wparam), LPARAM(lparam)) };
}

impl Injector {
    pub fn replay(&mut self, window: Window, event: InputEvent) {
        let Ok(frame) = window.rect() else { return };
        match event {
            InputEvent::PointerMove { x, y } => {
                (self.x, self.y) = (x, y);
                if let Some((target, cx, cy)) = self.target(window, frame) {
                    post(target, WM_MOUSEMOVE, self.buttons as usize, pack(cx, cy));
                }
            }
            InputEvent::Button { button, pressed } => self.button(window, frame, button, pressed),
            InputEvent::Scroll { dx, dy } => {
                let screen = self.screen(frame);
                let Some((target, _, _)) = self.target(window, frame) else { return };
                // Windows counts 120 per notch, positive up and right.
                if dy != 0.0 {
                    let delta = (-dy * 120.0) as i16 as u16 as usize;
                    post(
                        target,
                        WM_MOUSEWHEEL,
                        delta << 16 | self.buttons as usize,
                        pack(screen.x, screen.y),
                    );
                }
                if dx != 0.0 {
                    let delta = (dx * 120.0) as i16 as u16 as usize;
                    post(
                        target,
                        WM_MOUSEHWHEEL,
                        delta << 16 | self.buttons as usize,
                        pack(screen.x, screen.y),
                    );
                }
            }
            InputEvent::Key { code, pressed } => {
                self.focus(window);
                key(code, pressed);
            }
            InputEvent::Leave => {}
        }
    }

    fn screen(&self, frame: ghostportals_core::Rect) -> POINT {
        POINT { x: frame.x + self.x.round() as i32, y: frame.y + self.y.round() as i32 }
    }

    /// The deepest visible child window under the pointer, and the pointer in
    /// its client coordinates. `None` when the pointer is over the frame
    /// rather than the client area.
    fn target(&self, window: Window, frame: ghostportals_core::Rect) -> Option<(HWND, i32, i32)> {
        let screen = self.screen(frame);
        let top = window.hwnd();
        let mut client = RECT::default();
        unsafe { GetClientRect(top, &mut client) }.ok()?;
        let mut p = screen;
        let _ = unsafe { ScreenToClient(top, &mut p) };
        if p.x < client.left || p.y < client.top || p.x >= client.right || p.y >= client.bottom {
            return None;
        }
        let mut current = top;
        loop {
            let mut local = screen;
            let _ = unsafe { ScreenToClient(current, &mut local) };
            let child = unsafe {
                ChildWindowFromPointEx(
                    current,
                    local,
                    CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
                )
            };
            if child.is_invalid() || child == current {
                return Some((current, local.x, local.y));
            }
            current = child;
        }
    }

    fn button(
        &mut self,
        window: Window,
        frame: ghostportals_core::Rect,
        button: PointerButton,
        pressed: bool,
    ) {
        let (flag, down, up, xbutton) = match button {
            PointerButton::Left => (MK_LBUTTON, WM_LBUTTONDOWN, WM_LBUTTONUP, 0),
            PointerButton::Right => (MK_RBUTTON, WM_RBUTTONDOWN, WM_RBUTTONUP, 0),
            PointerButton::Middle => (MK_MBUTTON, WM_MBUTTONDOWN, WM_MBUTTONUP, 0),
            PointerButton::Back => (MK_XBUTTON1, WM_XBUTTONDOWN, WM_XBUTTONUP, 1),
            PointerButton::Forward => (MK_XBUTTON2, WM_XBUTTONDOWN, WM_XBUTTONUP, 2),
        };
        if pressed {
            self.buttons |= flag;
            self.focus(window);
        } else {
            self.buttons &= !flag;
        }
        match self.target(window, frame) {
            Some((target, cx, cy)) => {
                let wparam = (xbutton << 16) | self.buttons as usize;
                post(target, if pressed { down } else { up }, wparam, pack(cx, cy));
            }
            None if button == PointerButton::Left => self.title_bar(window, frame, pressed),
            None => {}
        }
    }

    /// Clicks on the frame: only the caption buttons mean anything, since
    /// moving goes through the proxy's own title strip.
    fn title_bar(&mut self, window: Window, frame: ghostportals_core::Rect, pressed: bool) {
        let screen = self.screen(frame);
        let mut hit = 0usize;
        let ok = unsafe {
            SendMessageTimeoutW(
                window.hwnd(),
                WM_NCHITTEST,
                WPARAM(0),
                LPARAM(pack(screen.x, screen.y)),
                SMTO_ABORTIFHUNG,
                100,
                Some(&mut hit),
            )
        };
        if ok.0 == 0 {
            return;
        }
        let hit = hit as u32;
        if pressed {
            self.nc_hit = Some(hit);
            return;
        }
        // Like a real caption button: act on release over the same button.
        if self.nc_hit.take() != Some(hit) || hit == HTCLIENT {
            return;
        }
        let command = match hit {
            HTCLOSE => SC_CLOSE,
            HTMINBUTTON => SC_MINIMIZE,
            HTMAXBUTTON if unsafe { IsZoomed(window.hwnd()) }.as_bool() => SC_RESTORE,
            HTMAXBUTTON => SC_MAXIMIZE,
            _ => return,
        };
        post(window.hwnd(), WM_SYSCOMMAND, command as usize, 0);
    }

    /// Makes the window the foreground window so it receives keys, working
    /// around the foreground lock by borrowing the current foreground
    /// thread's input state for a moment.
    fn focus(&mut self, window: Window) {
        if self.focused == Some(window) && Window::foreground() == Some(window) {
            return;
        }
        unsafe {
            let fg = GetForegroundWindow();
            let fg_thread = GetWindowThreadProcessId(fg, None);
            let ours = GetCurrentThreadId();
            let attached = fg_thread != 0
                && fg_thread != ours
                && AttachThreadInput(ours, fg_thread, true).as_bool();
            let _ = BringWindowToTop(window.hwnd());
            let ok = SetForegroundWindow(window.hwnd()).as_bool();
            if attached {
                let _ = AttachThreadInput(ours, fg_thread, false);
            }
            if !ok {
                debug!("could not bring {} to the foreground", window.id());
            }
        }
        self.focused = Some(window);
    }
}

fn key(code: u32, pressed: bool) {
    let Some((scan, extended)) = scancode_from_evdev(code) else {
        debug!("no Windows scan code for evdev key {code}");
        return;
    };
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wScan: scan,
                dwFlags: KEYBD_EVENT_FLAGS(flags.0),
                ..Default::default()
            },
        },
    };
    unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
}
