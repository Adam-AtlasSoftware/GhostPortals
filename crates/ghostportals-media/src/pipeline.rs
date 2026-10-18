use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ghostportals_core::{VideoFrame, VideoSender};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use tracing::{debug, info, warn};

/// Frames between forced keyframes. Short, because a receiver that joins or
/// loses a frame cannot show anything until the next one.
const GOP: u32 = 120;

/// What to capture.
#[derive(Debug, Clone, Copy)]
pub enum Source {
    /// A PipeWire stream by object serial (KWin, Mutter and portals all hand
    /// these out).
    PipeWire { serial: u64 },
    /// A window by HWND, through Windows.Graphics.Capture.
    Win32Window { hwnd: u64 },
    /// A moving test pattern.
    Test,
}

impl Source {
    fn description(&self) -> String {
        match self {
            // keepalive-time resends the last frame of an idle window, so a
            // receiver that just joined still gets a picture.
            Source::PipeWire { serial } => format!(
                "pipewiresrc target-object={serial} always-copy=true do-timestamp=true keepalive-time=500 \
                 ! videoconvert"
            ),
            Source::Win32Window { hwnd } => format!(
                "d3d11screencapturesrc window-handle={hwnd} capture-api=wgc show-border=false \
                 show-cursor=false ! d3d11download ! videoconvert"
            ),
            Source::Test => "videotestsrc is-live=true pattern=ball ! videoconvert".into(),
        }
    }
}

/// Encoders in order of preference, with the settings that make them
/// low-latency. Settings an element does not have are skipped.
fn encoder_candidates(kbps: u32) -> Vec<(&'static str, Vec<(&'static str, String)>)> {
    let k = kbps.to_string();
    let bps = (kbps * 1000).to_string();
    let gop = GOP.to_string();
    let nv = vec![
        ("tune", "ultra-low-latency".into()),
        ("zerolatency", "true".into()),
        ("zero-reorder-delay", "true".into()),
        ("rc-mode", "cbr".into()),
        ("bitrate", k.clone()),
        ("gop-size", gop.clone()),
        ("bframes", "0".into()),
    ];
    let software = [
        (
            "x264enc",
            vec![
                ("tune", "zerolatency".into()),
                ("speed-preset", "ultrafast".into()),
                ("bitrate", k.clone()),
                ("key-int-max", gop.clone()),
            ],
        ),
        (
            "openh264enc",
            vec![
                ("complexity", "low".into()),
                ("rate-control", "bitrate".into()),
                ("bitrate", bps),
                ("gop-size", gop.clone()),
            ],
        ),
    ];
    let mut list = if cfg!(windows) {
        vec![
            ("nvd3d11h264enc", nv),
            (
                "qsvh264enc",
                vec![
                    ("low-latency", "true".into()),
                    ("rate-control", "cbr".into()),
                    ("bitrate", k.clone()),
                    ("gop-size", gop.clone()),
                    ("b-frames", "0".into()),
                ],
            ),
            (
                "amfh264enc",
                vec![
                    ("usage", "ultra-low-latency".into()),
                    ("rate-control", "cbr".into()),
                    ("bitrate", k.clone()),
                    ("gop-size", gop.clone()),
                ],
            ),
            (
                "mfh264enc",
                vec![
                    ("low-latency", "true".into()),
                    ("rc-mode", "cbr".into()),
                    ("bitrate", k.clone()),
                    ("gop-size", gop.clone()),
                    ("bframes", "0".into()),
                ],
            ),
        ]
    } else {
        vec![
            ("nvh264enc", nv),
            (
                "vah264enc",
                vec![
                    ("rate-control", "cbr".into()),
                    ("bitrate", k.clone()),
                    ("key-int-max", gop.clone()),
                    ("b-frames", "0".into()),
                    ("target-usage", "7".into()),
                ],
            ),
            (
                "vaapih264enc",
                vec![
                    ("rate-control", "cbr".into()),
                    ("bitrate", k.clone()),
                    ("keyframe-period", gop.clone()),
                    ("max-bframes", "0".into()),
                ],
            ),
        ]
    };
    list.extend(software);
    list
}

fn decoder_candidates() -> &'static [&'static str] {
    if cfg!(windows) {
        &["d3d11h264dec", "nvh264dec", "avdec_h264", "openh264dec"]
    } else {
        &["nvh264dec", "vah264dec", "vaapih264dec", "avdec_h264", "openh264dec"]
    }
}

/// Sets each property the element has; logs and skips the rest.
fn configure(element: &gst::Element, props: &[(&str, String)]) {
    for (name, value) in props {
        let Some(pspec) = element.find_property(name) else { continue };
        match glib_value(value, &pspec) {
            Some(v) => element.set_property_from_value(name, &v),
            None => debug!("{}: cannot set {name}={value}", element.name()),
        }
    }
}

fn glib_value(value: &str, pspec: &gst::glib::ParamSpec) -> Option<gst::glib::Value> {
    gst::glib::Value::deserialize_with_pspec(value, pspec).ok()
}

fn make_first(names: &[&'static str]) -> Option<(gst::Element, &'static str)> {
    names.iter().find_map(|n| Some((gst::ElementFactory::make(n).build().ok()?, *n)))
}

/// Bitrate for a window of this size: about 0.1 bit per pixel per frame at
/// 60 fps, within sane bounds.
fn bitrate_kbps(width: i32, height: i32) -> u32 {
    let pixels = (width.max(1) as u64) * (height.max(1) as u64);
    ((pixels * 60 / 10_000) as u32).clamp(2_000, 40_000)
}

/// Even dimensions; H.264 encoders reject odd ones.
fn even(v: i32) -> i32 {
    (v.max(2) / 2) * 2
}

/// Watches a pipeline's bus on its own thread, calling `on_error` once per
/// error. Stops when the pipeline is dropped.
fn watch_bus(pipeline: &gst::Pipeline, label: String, on_error: Arc<dyn Fn() + Send + Sync>) {
    let bus = pipeline.bus().expect("pipelines have a bus");
    let weak = pipeline.downgrade();
    std::thread::spawn(move || {
        while weak.upgrade().is_some() {
            let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(250)) else { continue };
            match msg.view() {
                gst::MessageView::Error(e) => {
                    warn!("{label}: {} ({:?})", e.error(), e.debug());
                    on_error();
                }
                gst::MessageView::Warning(w) => debug!("{label}: {}", w.error()),
                gst::MessageView::Eos(_) => {
                    debug!("{label}: end of stream");
                    on_error();
                }
                _ => {}
            }
        }
    });
}

/// A running capture → H.264 pipeline feeding one stream of the link.
pub struct Encoder {
    pipeline: gst::Pipeline,
    encoder: gst::Element,
    pub name: &'static str,
    pub width: i32,
    pub height: i32,
}

impl Encoder {
    /// Starts capturing `source` scaled to `width`×`height` and sends each
    /// encoded frame as `stream`. `on_error` runs (on another thread) when the
    /// pipeline fails, e.g. because the captured window closed.
    pub fn start(
        source: Source,
        width: i32,
        height: i32,
        stream: u32,
        sender: VideoSender,
        on_error: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let (width, height) = (even(width), even(height));
        let candidates = encoder_candidates(bitrate_kbps(width, height));
        let names: Vec<&'static str> = candidates.iter().map(|(n, _)| *n).collect();
        let (encoder, name) =
            make_first(&names).ok_or_else(|| anyhow!("no H.264 encoder found"))?;
        let props = &candidates.iter().find(|(n, _)| *n == name).expect("listed").1;
        configure(&encoder, props);

        let head = gst::parse::bin_from_description(
            &format!(
                "{} ! videoscale ! capsfilter caps=\"video/x-raw,format=NV12,width={width},\
                 height={height},pixel-aspect-ratio=1/1\"",
                source.description()
            ),
            true,
        )
        .context("building the capture pipeline")?;
        let tail = gst::parse::bin_from_description(
            "h264parse config-interval=-1 \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false max-buffers=4",
            true,
        )?;
        let pipeline = gst::Pipeline::new();
        pipeline.add_many([head.upcast_ref::<gst::Element>(), &encoder, tail.upcast_ref()])?;
        gst::Element::link_many([head.upcast_ref::<gst::Element>(), &encoder, tail.upcast_ref()])?;

        let sink = tail
            .by_name("sink")
            .and_then(|s| s.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| anyhow!("appsink missing"))?;
        let started = Instant::now();
        let enc_weak = encoder.downgrade();
        let dropping = Arc::new(AtomicBool::new(false));
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                    let frame = VideoFrame {
                        stream,
                        keyframe,
                        pts_us: started.elapsed().as_micros() as u64,
                        data: map.as_slice().to_vec().into(),
                    };
                    // After a drop, the receiver cannot decode until the next
                    // keyframe; skip deltas until one comes and ask for it.
                    if dropping.load(Ordering::Relaxed) && !keyframe {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    if sender.send(frame) {
                        dropping.store(false, Ordering::Relaxed);
                    } else if !dropping.swap(true, Ordering::Relaxed) {
                        debug!("link busy, dropping video until the next keyframe");
                        if let Some(enc) = enc_weak.upgrade() {
                            request_keyframe(&enc);
                        }
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        watch_bus(&pipeline, format!("encoder ({name})"), Arc::new(on_error));
        pipeline.set_state(gst::State::Playing).context("starting the capture pipeline")?;
        info!("streaming {width}x{height} with {name}");
        Ok(Self { pipeline, encoder, name, width, height })
    }

    /// Makes the next frame a keyframe.
    pub fn force_keyframe(&self) {
        request_keyframe(&self.encoder);
    }
}

fn request_keyframe(encoder: &gst::Element) {
    let event = gst_video::DownstreamForceKeyUnitEvent::builder().all_headers(true).build();
    if let Some(pad) = encoder.static_pad("sink") {
        pad.send_event(event);
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Byte order of decoded frames handed to [`DecoderSink::Frames`].
#[derive(Debug, Clone, Copy)]
pub enum PixelFormat {
    /// For GTK textures.
    Rgba,
    /// For Windows DIBs.
    Bgra,
}

/// A decoded frame, four bytes per pixel in the requested [`PixelFormat`].
pub struct RgbaFrame {
    pub width: i32,
    pub height: i32,
    pub stride: usize,
    pub data: Vec<u8>,
}

/// Where decoded video goes.
pub enum DecoderSink {
    /// Each frame is handed to the callback (the proxy window).
    Frames(PixelFormat, Box<dyn Fn(RgbaFrame) + Send + Sync>),
    /// Counted and discarded (self-test).
    Count(Arc<AtomicU64>),
}

/// A running H.264 → display pipeline for one incoming stream.
pub struct Decoder {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    pub name: &'static str,
}

impl Decoder {
    /// `on_error` runs (on another thread) when decoding fails; the caller
    /// should ask the sender for a keyframe.
    pub fn start(sink: DecoderSink, on_error: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        let (decoder, name) =
            make_first(decoder_candidates()).ok_or_else(|| anyhow!("no H.264 decoder found"))?;
        let src = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder("video/x-h264")
                    .field("stream-format", "byte-stream")
                    .field("alignment", "au")
                    .build(),
            )
            .is_live(true)
            .format(gst::Format::Time)
            .do_timestamp(true)
            .max_bytes(32 << 20)
            .build();
        let parse = gst::ElementFactory::make("h264parse").build()?;
        let pipeline = gst::Pipeline::new();
        pipeline.add_many([src.upcast_ref::<gst::Element>(), &parse, &decoder])?;
        gst::Element::link_many([src.upcast_ref::<gst::Element>(), &parse, &decoder])?;

        match sink {
            DecoderSink::Frames(format, callback) => {
                let format = match format {
                    PixelFormat::Rgba => "RGBA",
                    PixelFormat::Bgra => "BGRA",
                };
                let tail = gst::parse::bin_from_description(
                    &format!(
                        "videoconvert ! capsfilter caps=\"video/x-raw,format={format}\" \
                         ! appsink name=sink sync=false max-buffers=1 drop=true"
                    ),
                    true,
                )?;
                pipeline.add(&tail)?;
                decoder.link(&tail)?;
                let appsink = tail
                    .by_name("sink")
                    .and_then(|s| s.downcast::<gst_app::AppSink>().ok())
                    .ok_or_else(|| anyhow!("appsink missing"))?;
                appsink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |sink| {
                            let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                            let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                            let info = gst_video::VideoInfo::from_caps(caps)
                                .map_err(|_| gst::FlowError::Error)?;
                            let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                            let frame =
                                gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                                    .map_err(|_| gst::FlowError::Error)?;
                            callback(RgbaFrame {
                                width: info.width() as i32,
                                height: info.height() as i32,
                                stride: frame.plane_stride()[0] as usize,
                                data: frame
                                    .plane_data(0)
                                    .map_err(|_| gst::FlowError::Error)?
                                    .to_vec(),
                            });
                            Ok(gst::FlowSuccess::Ok)
                        })
                        .build(),
                );
            }
            DecoderSink::Count(counter) => {
                let appsink = gst_app::AppSink::builder().sync(false).build();
                pipeline.add(&appsink)?;
                decoder.link(&appsink)?;
                appsink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |sink| {
                            let _ = sink.pull_sample();
                            counter.fetch_add(1, Ordering::Relaxed);
                            Ok(gst::FlowSuccess::Ok)
                        })
                        .build(),
                );
            }
        }

        watch_bus(&pipeline, format!("decoder ({name})"), Arc::new(on_error));
        pipeline.set_state(gst::State::Playing).context("starting the decoder")?;
        info!("decoding with {name}");
        Ok(Self { pipeline, src, name })
    }

    pub fn push(&self, frame: &VideoFrame) {
        let buffer = gst::Buffer::from_slice(frame.data.clone());
        if let Err(e) = self.src.push_buffer(buffer) {
            debug!("decoder not accepting frames: {e:?}");
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Saves one frame of `source` as a PNG, for checking what a capture sees.
pub fn snapshot(source: Source, width: i32, height: i32, path: &std::path::Path) -> Result<()> {
    crate::init()?;
    let pipeline = gst::parse::launch(&format!(
        "{} ! videoscale ! capsfilter caps=\"video/x-raw,width={},height={}\" ! videoconvert \
         ! pngenc snapshot=true ! filesink location=\"{}\"",
        source.description(),
        even(width),
        even(height),
        path.display()
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("not a pipeline"))?;
    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().expect("pipelines have a bus");
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(5),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let _ = pipeline.set_state(gst::State::Null);
    match msg.as_ref().map(|m| m.view()) {
        Some(gst::MessageView::Eos(_)) => Ok(()),
        Some(gst::MessageView::Error(e)) => bail!("snapshot failed: {}", e.error()),
        _ => bail!("no frame within 5 s"),
    }
}

/// Encodes a test pattern and decodes it again through a local channel,
/// proving the codecs and plugins this machine will use. Returns a summary.
pub fn selftest(seconds: u64) -> Result<String> {
    crate::init()?;
    let decoded = Arc::new(AtomicU64::new(0));
    let decoder = Decoder::start(DecoderSink::Count(decoded.clone()), || {})?;
    let (sender, mut rx) = VideoSender::channel(64);
    let encoder = Encoder::start(Source::Test, 640, 360, 1, sender, || {})?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut encoded = 0u64;
    let mut keyframes = 0u64;
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(frame) => {
                encoded += 1;
                keyframes += frame.keyframe as u64;
                decoder.push(&frame);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    let decoded = decoded.load(Ordering::Relaxed);
    let summary = format!(
        "encoder {} -> decoder {}: {encoded} frames encoded ({keyframes} keyframes), {decoded} decoded",
        encoder.name, decoder.name
    );
    drop(encoder);
    drop(decoder);
    if encoded == 0 || decoded == 0 {
        bail!("self-test failed: {summary}");
    }
    Ok(summary)
}
