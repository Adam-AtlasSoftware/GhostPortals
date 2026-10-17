// Ghost Portals bridge for GNOME Shell (Mutter).
//
// Serves io.ghostportals.Shell on the session bus so ghostportals-linux can
// list windows, follow their geometry and move them. Payloads are JSON
// strings to keep the interface stable as fields are added.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Meta from 'gi://Meta';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'io.ghostportals.Shell';
const OBJECT_PATH = '/io/ghostportals/Shell';
const IFACE_XML = `
<node>
  <interface name="io.ghostportals.Shell">
    <method name="ListWindows"><arg type="s" direction="out"/></method>
    <method name="DesktopBounds"><arg type="s" direction="out"/></method>
    <method name="MoveWindow">
      <arg type="s" name="id" direction="in"/>
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
    </method>
    <signal name="WindowChanged"><arg type="s"/></signal>
    <signal name="WindowRemoved"><arg type="s"/></signal>
    <signal name="DesktopChanged"><arg type="s"/></signal>
  </interface>
</node>`;

function isMove(op) {
    return op === Meta.GrabOp.MOVING || op === Meta.GrabOp.KEYBOARD_MOVING ||
        (Meta.GrabOp.MOVING_UNCONSTRAINED !== undefined && op === Meta.GrabOp.MOVING_UNCONSTRAINED);
}

export default class GhostPortalsExtension extends Extension {
    enable() {
        this._moving = null;
        this._windowSignals = new Map();
        this._signals = [];

        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this);
        this._dbus.export(Gio.DBus.session, OBJECT_PATH);
        this._nameId = Gio.bus_own_name_on_connection(
            Gio.DBus.session, BUS_NAME, Gio.BusNameOwnerFlags.NONE, null, null);

        const display = global.display;
        this._connect(display, 'window-created', (_d, w) => this._track(w));
        this._connect(display, 'grab-op-begin', (_d, w, op) => {
            if (w && isMove(op)) {
                this._moving = w;
                this._report(w);
            }
        });
        this._connect(display, 'grab-op-end', (_d, w) => {
            if (this._moving === w) {
                this._moving = null;
                this._report(w);
            }
        });
        this._connect(display, 'notify::focus-window', () => {
            if (display.focus_window)
                this._report(display.focus_window);
        });
        this._connect(global.backend.get_monitor_manager(), 'monitors-changed', () =>
            this._dbus.emit_signal('DesktopChanged', new GLib.Variant('(s)', [this.DesktopBounds()])));

        for (const actor of global.get_window_actors())
            this._track(actor.meta_window);
    }

    disable() {
        for (const [w, ids] of this._windowSignals)
            ids.forEach(id => w.disconnect(id));
        this._windowSignals.clear();
        this._signals.forEach(([obj, id]) => obj.disconnect(id));
        this._signals = [];
        Gio.bus_unown_name(this._nameId);
        this._dbus.unexport();
        this._dbus = null;
        this._moving = null;
    }

    // --- DBus methods ---

    ListWindows() {
        return JSON.stringify(this._windows().map(w => this._describe(w)));
    }

    DesktopBounds() {
        const {width, height} = global.display.get_size();
        return JSON.stringify({x: 0, y: 0, width, height});
    }

    MoveWindow(id, x, y) {
        const w = this._windows().find(w => this._id(w) === id);
        if (w)
            w.move_frame(true, x, y);
    }

    // --- internals ---

    _connect(obj, signal, fn) {
        this._signals.push([obj, obj.connect(signal, fn)]);
    }

    _windows() {
        return global.get_window_actors()
            .map(a => a.meta_window)
            .filter(w => w.get_window_type() === Meta.WindowType.NORMAL);
    }

    _id(w) {
        return String(w.get_id());
    }

    _describe(w) {
        const r = w.get_frame_rect();
        return {
            id: this._id(w),
            title: w.get_title() ?? '',
            app: w.get_wm_class() ?? '',
            x: r.x, y: r.y, width: r.width, height: r.height,
            active: w.has_focus(),
            moving: this._moving === w,
        };
    }

    _track(w) {
        if (this._windowSignals.has(w) || w.get_window_type() !== Meta.WindowType.NORMAL)
            return;
        const report = () => this._report(w);
        const ids = [
            w.connect('position-changed', report),
            w.connect('size-changed', report),
            w.connect('notify::title', report),
            w.connect('unmanaged', () => {
                this._windowSignals.delete(w);
                this._dbus?.emit_signal('WindowRemoved', new GLib.Variant('(s)', [this._id(w)]));
            }),
        ];
        this._windowSignals.set(w, ids);
        report();
    }

    _report(w) {
        this._dbus?.emit_signal('WindowChanged',
            new GLib.Variant('(s)', [JSON.stringify(this._describe(w))]));
    }
}
