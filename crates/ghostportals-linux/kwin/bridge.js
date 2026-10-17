// Ghost Portals bridge, loaded into KWin by ghostportals-linux.
//
// KWin scripts cannot serve DBus calls, so the bridge reports window geometry
// by calling the daemon and receives commands by long-polling NextCommand.

const SERVICE = "io.ghostportals.Linux";
const PATH = "/io/ghostportals/KWin";
const IFACE = "io.ghostportals.KWinBridge";

const moving = {};
let running = true;

function id(w) {
    return w.internalId.toString();
}

function report(w) {
    if (!running || !w.normalWindow) {
        return;
    }
    const g = w.frameGeometry;
    // Payloads go as JSON strings: QML numbers reach DBus as int or double
    // depending on their value, which would break a typed signature.
    callDBus(SERVICE, PATH, IFACE, "Update", JSON.stringify({
        id: id(w), title: w.caption || "", app: w.resourceClass || "",
        x: g.x, y: g.y, width: g.width, height: g.height,
        active: workspace.activeWindow === w, moving: !!moving[id(w)],
    }));
}

function reportScreen() {
    const g = workspace.virtualScreenGeometry;
    callDBus(SERVICE, PATH, IFACE, "Screen",
        JSON.stringify({ x: g.x, y: g.y, width: g.width, height: g.height }));
}

function track(w) {
    if (!w.normalWindow) {
        return;
    }
    w.frameGeometryChanged.connect(() => report(w));
    w.captionChanged.connect(() => report(w));
    w.interactiveMoveResizeStarted.connect(() => { moving[id(w)] = true; report(w); });
    w.interactiveMoveResizeFinished.connect(() => { delete moving[id(w)]; report(w); });
    report(w);
}

function find(windowId) {
    const list = workspace.windowList();
    for (let i = 0; i < list.length; i++) {
        if (id(list[i]) === windowId) {
            return list[i];
        }
    }
    return null;
}

function apply(cmd) {
    if (cmd.op === "move") {
        const w = find(cmd.id);
        if (w) {
            const g = w.frameGeometry;
            w.frameGeometry = { x: cmd.x, y: cmd.y, width: g.width, height: g.height };
        }
    } else if (cmd.op === "resync") {
        reportScreen();
        workspace.windowList().forEach(report);
    } else if (cmd.op === "quit") {
        running = false;
    }
}

function poll() {
    if (!running) {
        return;
    }
    callDBus(SERVICE, PATH, IFACE, "NextCommand", (reply) => {
        if (reply) {
            try {
                apply(JSON.parse(reply));
            } catch (e) {
                print("ghostportals: bad command " + reply + ": " + e);
            }
        }
        poll();
    });
}

workspace.windowAdded.connect(track);
workspace.windowRemoved.connect((w) => {
    if (running) {
        callDBus(SERVICE, PATH, IFACE, "Removed", id(w));
    }
});
workspace.windowActivated.connect((w) => { if (w) report(w); });
workspace.virtualScreenGeometryChanged.connect(reportScreen);

reportScreen();
workspace.windowList().forEach(track);
poll();
