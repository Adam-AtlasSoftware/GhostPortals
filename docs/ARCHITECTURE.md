# Architecture

## Virtual canvas

Both daemons work in one coordinate space. The Windows screen sits to the left
of the Linux screen, the bezel between them is `x = 0`, and `y = 0` is the top
of both screens. `ScreenEdge` (core `geometry.rs`) converts each side's local
desktop coordinates to and from this canvas:

- Windows: the bezel is the right edge of the virtual desktop
  (`SM_XVIRTUALSCREEN + SM_CXVIRTUALSCREEN`). The process is per-monitor DPI
  aware, so these are physical pixels.
- Linux: the bezel is the left edge of the compositor's desktop (the union of
  all outputs, in logical pixels).

The proxy is given the source window's exact size, so geometry matches
pixel for pixel. Both sides measure the **visible frame**, decorations
included:

- KWin `frameGeometry`, Hyprland `at`/`size`, and Mutter `get_frame_rect()`
  already give the visible frame.
- On Windows, `GetWindowRect` includes the invisible resize borders (about
  7 px left, right and bottom on Windows 10/11), so the daemon reads
  `DWMWA_EXTENDED_FRAME_BOUNDS` instead. When placing the proxy, it adds the
  proxy's own invisible borders (`Margins`, measured once before the proxy is
  ever clipped) back on, and offsets the clip region by them.

Mixed DPI between the two machines is not handled yet: a 1280 px window on one
side becomes a 1280 px proxy on the other, whatever the physical size of the
pixels.

## Handoff

```
 local window moves ─▶ HandoffTracker.sample ─▶ Sync / Release ─▶ link ─▶ peer
                                                                           │
               peer moves its proxy  ◀─ proxy_placement / proxy_clip ◀─────┘
```

- `HandoffTracker` (core) turns local samples into messages. It sends a `Sync`
  whenever the overhang or the drag state changes, and a `Release` once the
  window is fully back on its own side or has closed. It skips samples that
  have not changed.
- `proxy_placement` puts the peer's proxy so that exactly `visible_pixels` show
  next to the bezel: a Linux window entering Windows shows its left slice at
  the right edge, and a Windows window entering Linux shows its right slice at
  the left edge.
- The proxy is positioned, not cropped. Every compositor we target, and Windows,
  allows windows to hang off the outer edge of the desktop, so the part that
  has not crossed yet is simply off-screen. On Windows, `SetWindowRgn` also
  clips the proxy to the visible slice (`proxy_clip`), so nothing spills onto a
  monitor that might sit beyond the bezel.
- Each daemon ignores its own proxy when tracking local windows. The proxy
  hangs over the edge on purpose, and tracking it would echo it back to the
  peer.

- Moving works both ways. While the user drags a proxy (by its top 32 px,
  where the streamed title bar is), its daemon sends `ProxyMoved` with the
  proxy's canvas position, and the source moves the real window there. Proxy
  and real window share one canvas rectangle by construction, so no
  translation is needed. While the proxy is being dragged, its daemon ignores
  incoming placements so the two do not fight.

A window dropped mid-crossing simply stays there: the session, and with it the
stream, lasts until the window is fully back on its own side.

## Streaming

Remote desktop protocols were the original plan and do not fit. RDP RemoteApp
runs apps in a *separate* Windows session, and Windows 10/11 Pro and
Enterprise allow one interactive session, so connecting locks the physical
screen. X11 forwarding starts new app instances instead of showing the live
window, and cannot carry Wayland apps at all. So each daemon captures the
live window itself:

- **Windows**: `d3d11screencapturesrc` in Windows.Graphics.Capture mode,
  bound to the window's HWND. It captures the whole window surface even
  where it hangs off the screen.
- **KWin**: `zkde_screencast_unstable_v1.stream_window(uuid)`, which streams
  one window into PipeWire without a prompt, again including off-screen
  parts. KWin offers this protocol only to executables that a `.desktop` file
  grants it to, so the daemon writes one for itself
  (`backend/kwin_cast.rs`). Version 6 of the protocol reports the PipeWire
  *object serial*, which is what `pipewiresrc target-object` needs; passing
  the node id there instead silently connects to some other source, such as
  a webcam.
- **GNOME and Hyprland** are not done yet. The plan is Mutter's
  `org.gnome.Mutter.ScreenCast` `RecordWindow` and Hyprland's
  `hyprland-toplevel-export-v1`. The generic desktop portal is no help, since
  it asks the user to pick the window every time.

The media crate builds the pipelines on GStreamer and picks, at runtime, the
first H.264 encoder and decoder that exist on the machine (NVENC, QSV, AMF,
Media Foundation or VA-API before x264/OpenH264, and D3D11/NVDEC/VA before
libav), setting whichever low-latency properties each one has. Encoding uses
constant bitrate, no B-frames and a keyframe every 120 frames.

Frames travel on the control connection as binary frames (see below), in their
own 10-frame queue. When that queue is full a frame is dropped, the sender
skips deltas until the next keyframe and asks its encoder for one, so a slow
network costs frames, not latency. A receiver that joins mid-stream or fails
to decode sends `KeyframeRequest`.

The receiving daemon owns its proxy: a GTK 4 window on Linux (app id
`io.ghostportals.proxy`, which the backends recognise) and a Win32 popup on
Windows. Both get decoded frames as RGBA/BGRA and draw them themselves, which
keeps hit-testing in our hands for the drag strip and, next, input
forwarding.

## Input

The proxy reports pointer motion, buttons, wheel and keys as `Input`
messages in the real window's own coordinates (the proxy is the same size, so
proxy coordinates are window coordinates). Keys travel as Linux evdev codes;
Windows' set-1 scan codes match them except for the `E0`-prefixed keys,
mapped in core `keys.rs`. The source applies its own keyboard layout.

The proxy's top 32 px drag the proxy only once the pointer has moved 4 px
with the button held; a press and release without that is forwarded as a
click, so caption buttons still work.

**Windows replays** mouse input by posting `WM_MOUSEMOVE`/`WM_*BUTTON*`/
`WM_MOUSEWHEEL` to the deepest child window under the point
(`ChildWindowFromPointEx`). That works while the window hangs off the screen,
where the cursor cannot go, and never moves the cursor. Caption buttons are
recognised with `WM_NCHITTEST` and turned into `WM_SYSCOMMAND`. Keys go
through `SendInput` as scan codes after the window is brought to the
foreground (borrowing the foreground thread's input state with
`AttachThreadInput`), because posted key messages do not update the keyboard
state apps read for modifiers.

**KWin replays** through `org_kde_kwin_fake_input`, which drives the seat's
real pointer and keyboard. The pointer cannot leave the screens, so the
window is first moved fully on screen and focused, and goes back one second
after the pointer leaves the proxy ("control"). Input waits until KWin
reports the window at its new place with focus; injected any earlier, a click
lands on whatever was there before and the keys follow it.

KWin cannot replay input while another program holds its input capture.
`EisInputCaptureFilter` hands every pointer and key event to the capture
client, injected ones included. Synergy, Input Leap and Deskflow hold that
capture whenever the cursor is on the other computer, which is exactly when
the user is using a Linux window's proxy over there; replayed input would go
straight back to the other computer. So the daemon refuses while any capture
session exists and logs why. Making this direction work needs a way past the
seat: XTest for X11 (XWayland) apps, or a different input-sharing
arrangement.

## Wire protocol

TCP with `TCP_NODELAY`. Each frame is a 4-byte big-endian length, a tag byte,
and either one JSON `Message` (tag 0; core `protocol.rs`, tagged by `"type"`)
or one encoded video frame (tag 1: stream id, keyframe flag, timestamp, H.264
access unit). The `sync` body is exactly the payload from the original spec.
Control messages always go out before queued video.

Why this instead of WebSockets or gRPC:

- **Speed**: one small frame per change, no HTTP upgrade, no protobuf toolchain.
  On a LAN, latency is dominated by the 16 ms sampling, not by the transport.
- **Reliability**: the link (core `link.rs`) does a `Hello` handshake with a
  protocol version and an optional shared token, sends heartbeats every
  second, drops a peer that has been silent for 5 s, and reconnects with
  backoff. Messages queued while the peer is down are discarded: every `Sync`
  carries the full state, so stale ones would only make the proxy jump.
- **Growth**: unknown JSON fields are ignored, so new fields need no version
  bump. Because of the length framing, the body encoding can later switch to a
  binary format (postcard, protobuf) without touching the transport.

## Linux: why not a Wayland protocol

Wayland deliberately gives clients no access to other clients' windows. The
protocols that come closest do not help:

- `ext-foreign-toplevel-list-v1` lists toplevels but has no geometry and no way
  to move them. KWin only offers it to privileged clients.
- `wlr-foreign-toplevel-management-unstable-v1` (wlroots, Hyprland) can
  activate, minimize and close windows, but has no geometry either.
- `wp_viewporter` scales and crops the client's own surfaces. It cannot crop
  another application's window, such as the FreeRDP RemoteApp proxy.

So each compositor is reached through the privileged channel it already
provides, behind one `Backend` enum (`crates/ghostportals-linux/src/backend`):

| Compositor | Channel | Geometry | Drag detection | Move |
|---|---|---|---|---|
| KWin (Plasma 6) | KWin script loaded over DBus. It calls the daemon's `io.ghostportals.KWinBridge` and long-polls `NextCommand`, which returns every queued command and keeps only the newest placement per window | `frameGeometryChanged` signal | `interactiveMoveResizeStarted/Finished` | un-maximize and un-tile, then set `frameGeometry` |
| Hyprland | `.socket.sock` requests and `.socket2.sock` events | `j/clients`; `j/activewindow` polled every 16 ms (IPC sends nothing during a drag) | position changed within the last 150 ms | `setfloating` + `resizewindowpixel exact` + `movewindowpixel exact` |
| GNOME Shell (Mutter) | the bundled extension, serving `io.ghostportals.Shell` | `position-changed` / `size-changed` signals | `grab-op-begin/end` | un-maximize, then `Meta.Window.move_resize_frame` |

The KWin backend claims its bus name without letting other processes replace
it, so a second daemon fails to start instead of orphaning the first one's
script. `GHOSTPORTALS_INSTANCE` gives a development build separate names.

The payloads across these bridges are JSON strings, so new fields need no
changes to DBus signatures. This also sidesteps KWin's JavaScript numbers
arriving over DBus sometimes as `int` and sometimes as `double`.

Adding Sway or another wlroots compositor means adding a backend (Sway's IPC
reports geometry and supports `move position`).

## Windows

The daemon polls `GetForegroundWindow` and the window's visible frame every
16 ms. `GetGUIThreadInfo` with `GUI_INMOVESIZE` tells whether the user is
dragging. The proxy is the daemon's own `WS_POPUP` tool window, drawn with
`StretchDIBits`; its top strip answers `WM_NCHITTEST` with `HTCAPTION`, so
Windows drags it natively. It is placed with `SetWindowPos`
(`SWP_NOACTIVATE`) and clipped with `CreateRectRgn` + `SetWindowRgn`. Once
`SetWindowRgn` succeeds, the system owns the region.

The release zip bundles GStreamer: its DLLs beside the executable and the
plugins the daemon uses in `gstreamer\plugins`, which the media crate points
GStreamer at before initialising it. CI runs `selftest` from the unpacked zip
with the system GStreamer hidden from `PATH`.

`SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE)` would avoid polling and is the
natural next step. Polling was kept to match the spec and to stay simple.
