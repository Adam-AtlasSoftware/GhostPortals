//! The proxy window: a borderless popup that shows a window streamed from
//! Linux.
//!
//! It runs its own message loop on its own thread and draws the newest
//! decoded frame with GDI. The daemon talks to it through [`Proxy`], which is
//! cheap to clone and never blocks: frames go into a one-slot mailbox, and
//! everything else is posted to the window as messages. Pointer, wheel and
//! key input on the proxy comes back as [`InputEvent`]s for the daemon to
//! forward to the real window.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use ghostportals_core::keys::evdev_from_scancode;
use ghostportals_core::{InputEvent, PointerButton};
use ghostportals_media::RgbaFrame;
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, DIB_RGB_COLORS, EndPaint, InvalidateRect,
    PAINTSTRUCT, SRCCOPY, StretchDIBits,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_USERDATA, GetClientRect, GetMessageW,
    GetWindowLongPtrW, HTCAPTION, IDC_ARROW, LoadCursorW, MSG, PostMessageW, RegisterClassW,
    SW_HIDE, SW_SHOWNOACTIVATE, SendMessageW, SetWindowLongPtrW, SetWindowTextW, ShowWindow,
    TranslateMessage, WM_APP, WM_CLOSE, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCLBUTTONDOWN, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{HSTRING, w};

use crate::win32::Window;

/// Height of the strip along the top that drags the proxy, like a title bar.
/// The streamed window's own title bar is drawn there. A click there without
/// dragging still goes to the real window, for its title bar buttons.
const DRAG_STRIP: i32 = 32;

/// How far the pointer must move with the button down in the strip before it
/// counts as a drag rather than a click.
const DRAG_THRESHOLD: i32 = 4;

/// From Win32's commctrl range; not in the window-messaging bindings.
const WM_MOUSELEAVE: u32 = 0x02a3;

const WM_PROXY_FRAME: u32 = WM_APP + 1;
const WM_PROXY_SHOW: u32 = WM_APP + 2;
const WM_PROXY_HIDE: u32 = WM_APP + 3;

/// Shared between the daemon and the window procedure.
struct Shared {
    /// The newest frame; kept after painting so the window can repaint.
    frame: Mutex<Option<RgbaFrame>>,
    title: Mutex<String>,
    input: UnboundedSender<InputEvent>,
    /// A left press in the title strip, waiting to become a click or a drag.
    strip_press: Mutex<Option<(i32, i32)>>,
    /// Whether WM_MOUSELEAVE has been requested since the pointer came in.
    tracking: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub struct Proxy {
    hwnd: isize,
    shared: Arc<Shared>,
}

impl Proxy {
    /// Starts the window thread. Input on the proxy is sent to `input`.
    pub fn spawn(input: UnboundedSender<InputEvent>) -> Result<Self> {
        let shared = Arc::new(Shared {
            frame: Mutex::default(),
            title: Mutex::default(),
            input,
            strip_press: Mutex::default(),
            tracking: Default::default(),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("proxy-window".into())
            .spawn(move || match create(&thread_shared) {
                Ok(hwnd) => {
                    let _ = tx.send(Ok(hwnd.0 as isize));
                    pump();
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            })
            .context("starting the proxy window thread")?;
        let hwnd = rx.recv().context("proxy window thread died")??;
        Ok(Self { hwnd, shared })
    }

    pub fn window(&self) -> Window {
        Window::from_raw(self.hwnd)
    }

    pub fn show(&self, title: &str) {
        *self.shared.title.lock().unwrap() = format!("{title} (Ghost Portal)");
        self.post(WM_PROXY_SHOW);
    }

    pub fn hide(&self) {
        *self.shared.frame.lock().unwrap() = None;
        self.post(WM_PROXY_HIDE);
    }

    /// Replaces the frame on screen. Callable from any thread.
    pub fn frame(&self, frame: RgbaFrame) {
        *self.shared.frame.lock().unwrap() = Some(frame);
        self.post(WM_PROXY_FRAME);
    }

    fn post(&self, msg: u32) {
        let _ = unsafe { PostMessageW(Some(HWND(self.hwnd as _)), msg, WPARAM(0), LPARAM(0)) };
    }
}

fn create(shared: &Arc<Shared>) -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("GhostPortalsProxy");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            bail!("RegisterClassW: {}", windows::core::Error::from_thread());
        }
        // A tool window stays off the taskbar and Alt+Tab; the real window is
        // still listed on its own machine.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class,
            w!("Ghost Portal"),
            WS_POPUP,
            0,
            0,
            100,
            100,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        // The window procedure borrows `shared` through this pointer; the
        // Arc it points into lives as long as any Proxy handle does, and the
        // window lives for the whole process.
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::as_ptr(shared) as isize);
        std::mem::forget(shared.clone());
        Ok(hwnd)
    }
}

fn pump() {
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn shared(hwnd: HWND) -> Option<&'static Shared> {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Shared;
    // Safety: set once in `create` from a leaked Arc.
    unsafe { ptr.as_ref() }
}

fn point(lparam: LPARAM) -> (i32, i32) {
    ((lparam.0 & 0xffff) as i16 as i32, ((lparam.0 >> 16) & 0xffff) as i16 as i32)
}

/// Turns one input message on the proxy into events for the real window.
fn input(hwnd: HWND, s: &Shared, msg: u32, wparam: WPARAM, lparam: LPARAM) {
    let send = |e| {
        let _ = s.input.send(e);
    };
    let (x, y) = point(lparam);
    let at = || InputEvent::PointerMove { x: x as f64, y: y as f64 };
    let xbutton = || {
        if (wparam.0 >> 16) & 0xffff == 1 { PointerButton::Back } else { PointerButton::Forward }
    };
    match msg {
        WM_MOUSEMOVE => {
            if !s.tracking.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = unsafe { TrackMouseEvent(&mut tme) };
            }
            let pending = *s.strip_press.lock().unwrap();
            if let Some((px, py)) = pending {
                if (x - px).abs().max((y - py).abs()) >= DRAG_THRESHOLD {
                    // Hand the drag to Windows' own move loop; the daemon
                    // sees the move and drags the real window along.
                    *s.strip_press.lock().unwrap() = None;
                    unsafe {
                        let _ = ReleaseCapture();
                        SendMessageW(
                            hwnd,
                            WM_NCLBUTTONDOWN,
                            Some(WPARAM(HTCAPTION as usize)),
                            Some(LPARAM(0)),
                        );
                    }
                }
                return;
            }
            send(at());
        }
        WM_MOUSELEAVE => {
            s.tracking.store(false, std::sync::atomic::Ordering::Relaxed);
            send(InputEvent::Leave);
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
            unsafe { SetCapture(hwnd) };
            if msg == WM_LBUTTONDOWN && y < DRAG_STRIP {
                *s.strip_press.lock().unwrap() = Some((x, y));
                return;
            }
            let button = match msg {
                WM_LBUTTONDOWN => PointerButton::Left,
                WM_RBUTTONDOWN => PointerButton::Right,
                WM_MBUTTONDOWN => PointerButton::Middle,
                _ => xbutton(),
            };
            send(at());
            send(InputEvent::Button { button, pressed: true });
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => {
            let _ = unsafe { ReleaseCapture() };
            let button = match msg {
                WM_LBUTTONUP => PointerButton::Left,
                WM_RBUTTONUP => PointerButton::Right,
                WM_MBUTTONUP => PointerButton::Middle,
                _ => xbutton(),
            };
            // A strip press that never became a drag is a click after all.
            if msg == WM_LBUTTONUP && s.strip_press.lock().unwrap().take().is_some() {
                send(at());
                send(InputEvent::Button { button, pressed: true });
            }
            send(InputEvent::Button { button, pressed: false });
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let notches = ((wparam.0 >> 16) & 0xffff) as i16 as f64 / 120.0;
            // Windows: positive is up / right. The protocol: down / right.
            let (dx, dy) = if msg == WM_MOUSEWHEEL { (0.0, -notches) } else { (notches, 0.0) };
            send(InputEvent::Scroll { dx, dy });
        }
        _ => {
            let pressed = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            let scancode = ((lparam.0 >> 16) & 0xff) as u16;
            let extended = (lparam.0 >> 24) & 1 == 1;
            if let Some(code) = evdev_from_scancode(scancode, extended) {
                send(InputEvent::Key { code, pressed });
            }
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PROXY_FRAME => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_PROXY_SHOW => {
                if let Some(s) = shared(hwnd) {
                    let title = s.title.lock().unwrap().clone();
                    let _ = SetWindowTextW(hwnd, &HSTRING::from(title));
                }
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                LRESULT(0)
            }
            WM_PROXY_HIDE => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            // The stream decides when the proxy goes away.
            WM_CLOSE => LRESULT(0),
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP
            | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP | WM_MOUSELEAVE
            | WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN
            | WM_SYSKEYUP => {
                if let Some(s) = shared(hwnd) {
                    input(hwnd, s, msg, wparam, lparam);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                if let Some(s) = shared(hwnd)
                    && let Some(f) = s.frame.lock().unwrap().as_ref()
                {
                    let mut client = Default::default();
                    let _ = GetClientRect(hwnd, &mut client);
                    let info = BITMAPINFO {
                        bmiHeader: BITMAPINFOHEADER {
                            biSize: size_of::<BITMAPINFOHEADER>() as u32,
                            biWidth: (f.stride / 4) as i32,
                            // Negative: rows run top to bottom.
                            biHeight: -f.height,
                            biPlanes: 1,
                            biBitCount: 32,
                            biCompression: BI_RGB.0,
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    StretchDIBits(
                        hdc,
                        0,
                        0,
                        client.right - client.left,
                        client.bottom - client.top,
                        0,
                        0,
                        f.width,
                        f.height,
                        Some(f.data.as_ptr() as *const _),
                        &info,
                        DIB_RGB_COLORS,
                        SRCCOPY,
                    );
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
