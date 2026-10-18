//! Window streaming for Ghost Portals: capture a window, encode it as H.264,
//! and decode it again on the other machine, all on GStreamer.
//!
//! The best encoder and decoder present at runtime are picked from a
//! per-platform preference list (hardware first, software last), with
//! low-latency settings applied to whichever properties that element has.

mod pipeline;

pub use pipeline::{
    Decoder, DecoderSink, Encoder, PixelFormat, RgbaFrame, Source, selftest, snapshot,
};

use std::sync::Once;

use anyhow::{Context, Result};

/// Initializes GStreamer once per process. On Windows, a GStreamer bundle
/// shipped next to the executable takes precedence over any system install.
pub fn init() -> Result<()> {
    static BUNDLE: Once = Once::new();
    BUNDLE.call_once(use_bundle);
    gstreamer::init().context("initializing GStreamer")
}

/// Points GStreamer at `gstreamer/` beside the executable when it exists, so
/// the release zip runs without a GStreamer installation.
fn use_bundle() {
    let Some(dir) =
        std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("gstreamer")))
    else {
        return;
    };
    if !dir.is_dir() {
        return;
    }
    // Safety: runs once, before GStreamer or any other thread reads these.
    unsafe {
        std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", dir.join("plugins"));
        std::env::set_var("GST_PLUGIN_PATH_1_0", dir.join("plugins"));
        let scanner = dir.join("gst-plugin-scanner.exe");
        if scanner.exists() {
            std::env::set_var("GST_PLUGIN_SCANNER_1_0", scanner);
        }
        if let Some(cache) = std::env::var_os("LOCALAPPDATA") {
            let registry =
                std::path::Path::new(&cache).join("GhostPortals").join("gst-registry.bin");
            std::env::set_var("GST_REGISTRY_1_0", registry);
        }
    }
}
