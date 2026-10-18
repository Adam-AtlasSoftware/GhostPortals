//! The proxy window: a borderless popup that shows a window streamed from
//! Linux.
//!
//! It runs its own message loop on its own thread and draws the newest
//! decoded frame with GDI. The daemon talks to it through [`Proxy`], which is
//! cheap to clone and never blocks: frames go into a one-slot mailbox, and
//! everything else is posted to the window as messages.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use ghostportals_media::RgbaFrame;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, DIB_RGB_COLORS, EndPaint, InvalidateRect,
    PAINTSTRUCT, SRCCOPY, ScreenToClient, StretchDIBits,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_USERDATA, GetClientRect, GetMessageW,
    GetWindowLongPtrW, HTCAPTION, HTCLIENT, IDC_ARROW, LoadCursorW, MSG, PostMessageW,
    RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SetWindowLongPtrW, SetWindowTextW, ShowWindow,
    TranslateMessage, WM_APP, WM_CLOSE, WM_ERASEBKGND, WM_NCHITTEST, WM_PAINT, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{HSTRING, w};

use crate::win32::Window;

/// Height of the strip along the top that drags the proxy, like a title bar.
/// The streamed window's own title bar is drawn there.
const DRAG_STRIP: i32 = 32;

const WM_PROXY_FRAME: u32 = WM_APP + 1;
const WM_PROXY_SHOW: u32 = WM_APP + 2;
const WM_PROXY_HIDE: u32 = WM_APP + 3;

/// Shared between the daemon and the window procedure.
#[derive(Default)]
struct Shared {
    /// The newest frame; kept after painting so the window can repaint.
    frame: Mutex<Option<RgbaFrame>>,
    title: Mutex<String>,
}

#[derive(Clone)]
pub struct Proxy {
    hwnd: isize,
    shared: Arc<Shared>,
}

impl Proxy {
    pub fn spawn() -> Result<Self> {
        let shared = Arc::new(Shared::default());
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
            WM_NCHITTEST => {
                let hit = DefWindowProcW(hwnd, msg, wparam, lparam);
                if hit.0 as u32 != HTCLIENT {
                    return hit;
                }
                let mut p = POINT {
                    x: (lparam.0 & 0xffff) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xffff) as i16 as i32,
                };
                let _ = ScreenToClient(hwnd, &mut p);
                if p.y < DRAG_STRIP { LRESULT(HTCAPTION as isize) } else { hit }
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
