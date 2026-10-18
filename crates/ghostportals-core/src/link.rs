//! A self-healing connection to the peer daemon.
//!
//! [`Link::spawn`] runs a background task that listens or dials, performs the
//! [`Hello`] handshake, keeps the connection alive with heartbeats and
//! reconnects when it drops. Callers just send [`Message`]s and receive
//! [`LinkEvent`]s.
//!
//! Messages sent while no peer is connected are discarded: every sync packet
//! carries the full current state, so replaying stale ones would only make the
//! proxy window jump.
//!
//! Every frame starts with a tag byte: [`TAG_MESSAGE`] frames carry one JSON
//! [`Message`], [`TAG_VIDEO`] frames one encoded [`VideoFrame`]. Video shares
//! the connection so the daemons need only one port, and has its own small
//! queue that drops instead of waiting, so a slow network never delays sync
//! messages behind a backlog of stale video.

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep, timeout};
use tokio_util::bytes::Bytes;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{debug, info, warn};

use crate::protocol::{Hello, Message, PROTOCOL_VERSION};

/// Large enough for a keyframe of a big window at a generous bitrate.
const MAX_FRAME: usize = 16 << 20;
const TAG_MESSAGE: u8 = 0;
const TAG_VIDEO: u8 = 1;
/// About a sixth of a second at 60 fps.
const VIDEO_QUEUE: usize = 10;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const HEARTBEAT: Duration = Duration::from_secs(1);
const PEER_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub enum Endpoint {
    Listen(SocketAddr),
    Connect(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkEvent {
    Connected(Hello),
    Message(Message),
    Video(VideoFrame),
    Disconnected,
}

/// One encoded access unit of a window stream.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoFrame {
    pub stream: u32,
    pub keyframe: bool,
    /// Capture time in microseconds, on the sender's clock.
    pub pts_us: u64,
    pub data: Bytes,
}

impl VideoFrame {
    const HEADER: usize = 1 + 4 + 1 + 8;

    fn encode(&self) -> Bytes {
        let mut out = Vec::with_capacity(Self::HEADER + self.data.len());
        out.push(TAG_VIDEO);
        out.extend_from_slice(&self.stream.to_be_bytes());
        out.push(self.keyframe as u8);
        out.extend_from_slice(&self.pts_us.to_be_bytes());
        out.extend_from_slice(&self.data);
        out.into()
    }

    fn decode(frame: &[u8]) -> Option<Self> {
        if frame.len() < Self::HEADER || frame[0] != TAG_VIDEO {
            return None;
        }
        Some(Self {
            stream: u32::from_be_bytes(frame[1..5].try_into().ok()?),
            keyframe: frame[5] != 0,
            pts_us: u64::from_be_bytes(frame[6..14].try_into().ok()?),
            data: Bytes::copy_from_slice(&frame[Self::HEADER..]),
        })
    }
}

/// Hands encoded video to the link from any thread (a GStreamer streaming
/// thread, typically).
#[derive(Debug, Clone)]
pub struct VideoSender(mpsc::Sender<VideoFrame>);

impl VideoSender {
    /// A sender not tied to a link, for local pipelines and tests.
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<VideoFrame>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self(tx), rx)
    }

    /// Queues a frame. Returns false when it was dropped because the queue is
    /// full; the encoder should then force a keyframe so the receiver can
    /// recover.
    pub fn send(&self, frame: VideoFrame) -> bool {
        self.0.try_send(frame).is_ok()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad frame: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("bad frame tag {0}")]
    Tag(u8),
    #[error("peer closed the connection")]
    Closed,
    #[error("no hello from peer within {0:?}")]
    HandshakeTimeout(Duration),
    #[error("expected hello, got {0:?}")]
    NoHello(Message),
    #[error("peer speaks protocol v{0}, we speak v{PROTOCOL_VERSION}")]
    Version(u16),
    #[error("peer sent a wrong or missing token")]
    Token,
    #[error("peer silent for {0:?}")]
    PeerTimeout(Duration),
}

pub struct Link {
    tx: mpsc::Sender<Message>,
    video: VideoSender,
    rx: mpsc::Receiver<LinkEvent>,
}

impl Link {
    /// Starts the background task. `expected_token`, when set, must match the
    /// token in the peer's hello. Fails straight away if a listening endpoint
    /// cannot be bound (another daemon already has the port, say); dialling
    /// never fails here, it retries in the background.
    pub async fn spawn(
        endpoint: Endpoint,
        hello: Hello,
        expected_token: Option<String>,
    ) -> std::io::Result<Self> {
        let listener = match &endpoint {
            Endpoint::Listen(addr) => {
                let l = TcpListener::bind(addr).await?;
                info!("listening on {addr}");
                Some(l)
            }
            Endpoint::Connect(_) => None,
        };
        let (out_tx, out_rx) = mpsc::channel(64);
        let (video_tx, video_rx) = mpsc::channel(VIDEO_QUEUE);
        let (ev_tx, ev_rx) = mpsc::channel(64);
        tokio::spawn(run(endpoint, listener, hello, expected_token, out_rx, video_rx, ev_tx));
        Ok(Self { tx: out_tx, video: VideoSender(video_tx), rx: ev_rx })
    }

    /// Queues a message for the peer. Never blocks the caller; when the queue
    /// is full the message is dropped, since newer state is on its way.
    pub fn send(&self, msg: Message) {
        if self.tx.try_send(msg).is_err() {
            debug!("link queue full, dropping message");
        }
    }

    pub fn video_sender(&self) -> VideoSender {
        self.video.clone()
    }

    pub async fn recv(&mut self) -> Option<LinkEvent> {
        self.rx.recv().await
    }
}

type Conn = Framed<TcpStream, LengthDelimitedCodec>;

async fn run(
    endpoint: Endpoint,
    listener: Option<TcpListener>,
    hello: Hello,
    expected_token: Option<String>,
    mut out_rx: mpsc::Receiver<Message>,
    mut video_rx: mpsc::Receiver<VideoFrame>,
    ev_tx: mpsc::Sender<LinkEvent>,
) {
    let mut backoff = Duration::from_millis(250);
    loop {
        let stream = match (&endpoint, &listener) {
            (_, Some(l)) => match l.accept().await {
                Ok((s, peer)) => {
                    info!("peer {peer} connected");
                    s
                }
                Err(e) => {
                    warn!("accept failed: {e}");
                    sleep(backoff).await;
                    continue;
                }
            },
            (Endpoint::Connect(addr), None) => match TcpStream::connect(addr).await {
                Ok(s) => {
                    info!("connected to {addr}");
                    s
                }
                Err(e) => {
                    debug!("connect to {addr} failed: {e}; retrying in {backoff:?}");
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            },
            (Endpoint::Listen(_), None) => unreachable!(),
        };
        if let Err(e) = stream.set_nodelay(true) {
            warn!("TCP_NODELAY: {e}");
        }
        let mut conn = Framed::new(
            stream,
            LengthDelimitedCodec::builder().max_frame_length(MAX_FRAME).new_codec(),
        );

        let peer = match handshake(&mut conn, &hello, expected_token.as_deref()).await {
            Ok(peer) => peer,
            Err(e) => {
                warn!("handshake failed: {e}");
                sleep(backoff).await;
                continue;
            }
        };
        backoff = Duration::from_millis(250);
        while out_rx.try_recv().is_ok() {}
        while video_rx.try_recv().is_ok() {}
        if ev_tx.send(LinkEvent::Connected(peer)).await.is_err() {
            return;
        }

        let result = session(&mut conn, &mut out_rx, &mut video_rx, &ev_tx).await;
        match result {
            Ok(()) => return, // the owning Link was dropped
            Err(e) => warn!("link lost: {e}"),
        }
        if ev_tx.send(LinkEvent::Disconnected).await.is_err() {
            return;
        }
    }
}

async fn send(conn: &mut Conn, msg: &Message) -> Result<(), LinkError> {
    let mut frame = vec![TAG_MESSAGE];
    serde_json::to_writer(&mut frame, msg)?;
    conn.send(Bytes::from(frame)).await?;
    Ok(())
}

enum Incoming {
    Message(Message),
    Video(VideoFrame),
}

async fn next(conn: &mut Conn) -> Result<Incoming, LinkError> {
    let frame = conn.next().await.ok_or(LinkError::Closed)??;
    match frame.first() {
        Some(&TAG_MESSAGE) => Ok(Incoming::Message(serde_json::from_slice(&frame[1..])?)),
        Some(&TAG_VIDEO) => {
            VideoFrame::decode(&frame).map(Incoming::Video).ok_or(LinkError::Tag(TAG_VIDEO))
        }
        Some(&tag) => Err(LinkError::Tag(tag)),
        None => Err(LinkError::Tag(0xff)),
    }
}

async fn handshake(
    conn: &mut Conn,
    hello: &Hello,
    expected_token: Option<&str>,
) -> Result<Hello, LinkError> {
    send(conn, &Message::Hello(hello.clone())).await?;
    let incoming = timeout(HANDSHAKE_TIMEOUT, next(conn))
        .await
        .map_err(|_| LinkError::HandshakeTimeout(HANDSHAKE_TIMEOUT))??;
    let Incoming::Message(msg) = incoming else {
        return Err(LinkError::Tag(TAG_VIDEO));
    };
    let Message::Hello(peer) = msg else {
        return Err(LinkError::NoHello(msg));
    };
    if peer.protocol_version != PROTOCOL_VERSION {
        return Err(LinkError::Version(peer.protocol_version));
    }
    if let Some(expected) = expected_token
        && peer.token.as_deref() != Some(expected)
    {
        return Err(LinkError::Token);
    }
    info!("peer is {} ({:?})", peer.host, peer.side);
    Ok(peer)
}

/// Runs one connected session. `Ok` means the caller went away and the task
/// should stop; `Err` means the connection should be re-established.
async fn session(
    conn: &mut Conn,
    out_rx: &mut mpsc::Receiver<Message>,
    video_rx: &mut mpsc::Receiver<VideoFrame>,
    ev_tx: &mpsc::Sender<LinkEvent>,
) -> Result<(), LinkError> {
    let mut heartbeat = interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut seq = 0u64;
    let mut last_seen = Instant::now();

    loop {
        tokio::select! {
            // Control messages first: a sync must never wait behind video.
            biased;
            out = out_rx.recv() => match out {
                Some(msg) => send(conn, &msg).await?,
                None => return Ok(()),
            },
            Some(frame) = video_rx.recv() => conn.send(frame.encode()).await?,
            incoming = next(conn) => {
                last_seen = Instant::now();
                let event = match incoming? {
                    Incoming::Message(Message::Ping { seq }) => {
                        send(conn, &Message::Pong { seq }).await?;
                        continue;
                    }
                    Incoming::Message(Message::Pong { .. }) => continue,
                    Incoming::Message(Message::Hello(_)) => {
                        debug!("ignoring repeated hello");
                        continue;
                    }
                    Incoming::Message(msg) => LinkEvent::Message(msg),
                    Incoming::Video(frame) => LinkEvent::Video(frame),
                };
                if ev_tx.send(event).await.is_err() {
                    return Ok(());
                }
            }
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > PEER_TIMEOUT {
                    return Err(LinkError::PeerTimeout(PEER_TIMEOUT));
                }
                seq += 1;
                send(conn, &Message::Ping { seq }).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Side;
    use crate::protocol::{Direction, EdgeDelta, SyncPacket, WindowDim};

    fn hello(side: Side, token: Option<&str>) -> Hello {
        Hello {
            protocol_version: PROTOCOL_VERSION,
            side,
            host: format!("{side:?}"),
            token: token.map(str::to_owned),
        }
    }

    async fn free_port() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    }

    async fn expect(link: &mut Link) -> LinkEvent {
        timeout(Duration::from_secs(5), link.recv()).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn round_trip_and_reconnect() {
        let addr = free_port().await;
        let mut server =
            Link::spawn(Endpoint::Listen(addr), hello(Side::Linux, None), Some("s3cret".into()))
                .await
                .unwrap();
        let mut client = Link::spawn(
            Endpoint::Connect(addr.to_string()),
            hello(Side::Windows, Some("s3cret")),
            None,
        )
        .await
        .unwrap();

        let LinkEvent::Connected(peer) = expect(&mut server).await else { panic!() };
        assert_eq!(peer.side, Side::Windows);
        let LinkEvent::Connected(peer) = expect(&mut client).await else { panic!() };
        assert_eq!(peer.side, Side::Linux);

        let msg = Message::Sync(SyncPacket {
            session_id: "handoff-1".into(),
            direction: Direction::WindowsToLinux,
            is_dragging: true,
            window_dim: WindowDim { width: 640, height: 480 },
            edge_delta: EdgeDelta { visible_pixels: 12, offset_y: 34 },
        });
        client.send(msg.clone());
        assert_eq!(expect(&mut server).await, LinkEvent::Message(msg));

        let frame = VideoFrame {
            stream: 7,
            keyframe: true,
            pts_us: 123_456,
            data: Bytes::from_static(&[0, 0, 0, 1, 0x65, 0xaa]),
        };
        assert!(client.video_sender().send(frame.clone()));
        assert_eq!(expect(&mut server).await, LinkEvent::Video(frame));

        // Drop the client; the server notices and a new client gets through.
        drop(client);
        assert_eq!(expect(&mut server).await, LinkEvent::Disconnected);
        let mut client = Link::spawn(
            Endpoint::Connect(addr.to_string()),
            hello(Side::Windows, Some("s3cret")),
            None,
        )
        .await
        .unwrap();
        assert!(matches!(expect(&mut server).await, LinkEvent::Connected(_)));
        assert!(matches!(expect(&mut client).await, LinkEvent::Connected(_)));
    }

    #[tokio::test]
    async fn wrong_token_is_rejected() {
        let addr = free_port().await;
        let mut server =
            Link::spawn(Endpoint::Listen(addr), hello(Side::Linux, None), Some("right".into()))
                .await
                .unwrap();
        let _client = Link::spawn(
            Endpoint::Connect(addr.to_string()),
            hello(Side::Windows, Some("wrong")),
            None,
        )
        .await
        .unwrap();
        assert!(timeout(Duration::from_millis(800), server.recv()).await.is_err());
    }
}
