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

Mixed DPI between the two machines is not handled yet: a 1280 px window on one
side is placed as a 1280 px proxy on the other.

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

Still open: what happens when a drag ends mid-crossing (`is_dragging: false`
with a non-zero overhang). Possible answers are snapping back, or committing
the handoff by hiding the real window and keeping the proxy. The peer already
receives that state.

## Wire protocol

TCP with `TCP_NODELAY`. Each frame is a 4-byte big-endian length followed by
one JSON `Message` (core `protocol.rs`), tagged by `"type"`. The `sync` body is
exactly the payload from the original spec.

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
| KWin (Plasma 6) | KWin script loaded over DBus. It calls the daemon's `io.ghostportals.KWinBridge` and long-polls `NextCommand` for moves | `frameGeometryChanged` signal | `interactiveMoveResizeStarted/Finished` | sets `frameGeometry` |
| Hyprland | `.socket.sock` requests and `.socket2.sock` events | `j/clients`; `j/activewindow` polled every 16 ms (IPC sends nothing during a drag) | position changed within the last 150 ms | `setfloating` + `movewindowpixel exact` |
| GNOME Shell (Mutter) | the bundled extension, serving `io.ghostportals.Shell` | `position-changed` / `size-changed` signals | `grab-op-begin/end` | `Meta.Window.move_frame` |

The payloads across these bridges are JSON strings, so new fields need no
changes to DBus signatures. This also sidesteps KWin's JavaScript numbers
arriving over DBus sometimes as `int` and sometimes as `double`.

Adding Sway or another wlroots compositor means adding a backend (Sway's IPC
reports geometry and supports `move position`).

## Windows

The daemon polls `GetForegroundWindow` and `GetWindowRect` every 16 ms.
`GetGUIThreadInfo` with `GUI_INMOVESIZE` tells whether the user is dragging.
The proxy is found by title with `EnumWindows`, placed with `SetWindowPos`
(`SWP_NOACTIVATE`), and clipped with `CreateRectRgn` + `SetWindowRgn`. Once
`SetWindowRgn` succeeds, the system owns the region.

`SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE)` would avoid polling and is the
natural next step. Polling was kept to match the spec and to stay simple.
