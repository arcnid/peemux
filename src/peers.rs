// Tailscale peer discovery + direct peemux messaging.
//
// Each peemux instance listens on TCP 9867. A background thread polls
// `tailscale status --json` for online peers, then probes each on 9867.
// Peers that respond with a Hello handshake appear in the friends list.
// Messages flow directly over TCP — no external service needed.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_PORT: u16 = 9867;
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Cadence of the sharer-side frame poll: capture, hash, send-if-changed.
const FRAME_INTERVAL: Duration = Duration::from_millis(100);
/// Faster poll right after remote input, so the viewer sees their own
/// keystrokes echo without waiting out the idle cadence.
const FRAME_INTERVAL_HOT: Duration = Duration::from_millis(33);
/// How long one remote keystroke keeps the frame poll in the hot cadence.
const INPUT_HOT_WINDOW: Duration = Duration::from_millis(1500);
/// Hard cap on a single viewer input message. Enforced on the READ itself
/// (not after buffering) so a peer can't stream an unbounded newline-less
/// line and OOM the sharer. A line over this drops the connection.
const MAX_INPUT_LINE: usize = 32 * 1024;
/// Coarse per-connection input rate cap, enforced in the sharer's input
/// thread BEFORE events hit the unbounded channel — so a flood is dropped at
/// ingest, not after ballooning the queue. main.rs re-checks per pane.
const MAX_INPUT_BYTES_PER_SEC: usize = 64 * 1024;

/// TCP port for peer traffic. `PEEMUX_PORT` env overrides the default —
/// hidden knob so two instances can coexist on one box for local testing.
pub fn peemux_port() -> u16 {
    static PORT: OnceLock<u16> = OnceLock::new();
    *PORT.get_or_init(|| {
        std::env::var("PEEMUX_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_PORT)
    })
}

// ─── public types ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Peer {
    pub hostname: String,
    pub display_name: String,
    pub tailscale_ip: String,
    pub has_unread: bool,
}

pub enum PeerEvent {
    PeersUpdated { peers: Vec<Peer>, self_ip: Option<String> },
    IncomingMessage { from: String, text: String },
    /// A peer offered to share a pane with us. `host`/`token` are what we
    /// need to join; the toast + Ctrl-b y flow lives in main.rs.
    ShareOffer { from: String, title: String, host: String, token: String, writable: bool },
    /// A viewer connected to one of our outgoing shares.
    ViewerJoined { viewer: String, title: String },
    /// A viewer disconnected from one of our outgoing shares.
    ViewerLeft { viewer: String, title: String },
    /// A viewer typed into one of our writable shares. main.rs sanitizes and
    /// routes the bytes into the pane's PTY — the peer threads never touch it.
    ViewerInput { token: String, viewer: String, data: String },
    Error(String),
}

// ─── pane sharing: outgoing share registry ─────────────────────────────────
//
// The sharer registers each active share here (token → entry). The listener
// thread validates incoming JoinShare tokens against this map and runs the
// per-viewer streaming loop. main.rs owns insertion/removal; entries carry a
// capture closure so this module never needs to know about Window/VtState.

/// One active outgoing share. `capture` returns (rows, cols, full ANSI
/// snapshot) of the shared pane, or None once the pane is gone.
#[derive(Clone)]
pub struct ShareEntry {
    pub title: String,
    pub capture: Arc<dyn Fn() -> Option<(u16, u16, String)> + Send + Sync>,
    /// Set true to end the share — every viewer loop sends ShareEnd and closes.
    pub stop: Arc<AtomicBool>,
    /// Viewers may type into the shared PTY. Flipped live by grant-write /
    /// revoke-write; stream loops notice and send WriteStatus to viewers.
    pub writable: Arc<AtomicBool>,
    /// Last time a viewer typed — drives the hot frame cadence so remote
    /// keystrokes echo fast. None until the first input.
    pub last_input: Arc<Mutex<Option<std::time::Instant>>>,
    /// Display names of connected viewers (for `peemux share list`).
    pub viewers: Arc<Mutex<Vec<String>>>,
}

pub type ShareRegistry = Arc<Mutex<HashMap<String, ShareEntry>>>;

/// Random alphanumeric capability token (24 chars) from /dev/urandom.
pub fn random_token() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 24];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        // Fallback entropy — still unguessable enough for a tailnet-scoped
        // capability, but /dev/urandom should never fail on unix.
        let mut h = DefaultHasher::new();
        std::time::SystemTime::now().hash(&mut h);
        std::process::id().hash(&mut h);
        let mut seed = h.finish();
        for b in bytes.iter_mut() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *b = (seed >> 33) as u8;
        }
    }
    bytes.iter().map(|b| CHARSET[(*b as usize) % CHARSET.len()] as char).collect()
}

// ─── wire protocol (newline-delimited JSON on TCP 9867) ───────────────────

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Wire {
    Hello { user: String },
    Message { from: String, text: String },
    Ack,
    // Pane sharing. All additive — old peers fail to parse the unknown
    // `type` tag, drop the connection, and are otherwise unaffected. New
    // FIELDS on existing tags use #[serde(default)] so old sharers (who
    // omit them) still parse; serde ignores unknown fields on old viewers.
    ShareOffer {
        from: String,
        title: String,
        host: String,
        token: String,
        #[serde(default)]
        writable: bool,
    },
    /// Sent by a viewer; the connection then STAYS OPEN as the frame stream.
    JoinShare { token: String, user: String },
    ShareAccepted {
        title: String,
        rows: u16,
        cols: u16,
        #[serde(default)]
        writable: bool,
    },
    /// Full ANSI screen snapshot of the shared pane.
    PaneFrame { rows: u16, cols: u16, seq: String },
    ShareEnd { reason: String },
    ShareError { message: String },
    /// Viewer → sharer on the frame-stream connection: keystrokes for the
    /// shared PTY (UTF-8; key encodings are plain-text-safe in JSON).
    Input { data: String },
    /// Sharer → viewer: write access granted/revoked mid-share.
    WriteStatus { writable: bool },
}

// ─── tailscale status JSON ────────────────────────────────────────────────

#[derive(Deserialize)]
struct TsStatus {
    #[serde(rename = "Peer")]
    peer: Option<HashMap<String, TsNode>>,
    #[serde(rename = "Self")]
    self_node: Option<TsNode>,
}

#[derive(Deserialize)]
struct TsNode {
    #[serde(rename = "HostName")]
    host_name: Option<String>,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
    #[serde(rename = "Online")]
    online: Option<bool>,
}

/// (online peers as (hostname, ip), our own tailscale IP if known)
fn get_tailscale_peers() -> (Vec<(String, String)>, Option<String>) {
    let output = Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .or_else(|_| {
            Command::new("/Applications/Tailscale.app/Contents/MacOS/Tailscale")
                .args(["status", "--json"])
                .output()
        });
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return (vec![], None),
    };
    let status: TsStatus = match serde_json::from_slice(&output.stdout) {
        Ok(s) => s,
        Err(_) => return (vec![], None),
    };
    let self_ip = status
        .self_node
        .and_then(|n| n.tailscale_ips)
        .and_then(|ips| ips.into_iter().find(|ip| ip.starts_with("100.")));
    let peers = status
        .peer
        .unwrap_or_default()
        .into_values()
        .filter(|n| n.online.unwrap_or(false))
        .filter_map(|n| {
            let hostname = n.host_name?;
            let ip = n.tailscale_ips?.into_iter().find(|ip| ip.starts_with("100."))?;
            Some((hostname, ip))
        })
        .collect();
    (peers, self_ip)
}

fn probe_peemux(ip: &str, my_name: &str) -> Option<String> {
    let addr: SocketAddr = format!("{ip}:{}", peemux_port()).parse().ok()?;
    let stream = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(PROBE_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(PROBE_TIMEOUT)).ok()?;

    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut writer = stream;

    let hello = Wire::Hello { user: my_name.to_string() };
    writeln!(writer, "{}", serde_json::to_string(&hello).ok()?).ok()?;
    writer.flush().ok()?;

    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    match serde_json::from_str::<Wire>(line.trim()) {
        Ok(Wire::Hello { user }) => Some(user),
        _ => None,
    }
}

// ─── lifecycle ────────────────────────────────────────────────────────────

pub fn spawn_peer_system(display_name: String, shares: ShareRegistry) -> mpsc::Receiver<PeerEvent> {
    let (tx, rx) = mpsc::channel();
    let port = peemux_port();

    // Listener thread: accept incoming connections on the peemux port.
    let tx_listen = tx.clone();
    let listen_name = display_name.clone();
    thread::spawn(move || {
        let listener = match TcpListener::bind(format!("0.0.0.0:{port}")) {
            Ok(l) => l,
            Err(e) => {
                let _ = tx_listen.send(PeerEvent::Error(format!("bind :{port}: {e}")));
                return;
            }
        };
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let tx = tx_listen.clone();
            let name = listen_name.clone();
            let shares = shares.clone();
            thread::spawn(move || {
                let _ = handle_incoming(stream, &name, &tx, &shares);
            });
        }
    });

    // Discovery thread: poll tailscale, probe peers.
    thread::spawn(move || loop {
        let (ts_peers, self_ip) = get_tailscale_peers();
        let mut peers = Vec::new();
        for (hostname, ip) in &ts_peers {
            if let Some(name) = probe_peemux(ip, &display_name) {
                peers.push(Peer {
                    hostname: hostname.clone(),
                    display_name: name,
                    tailscale_ip: ip.clone(),
                    has_unread: false,
                });
            }
        }
        let _ = tx.send(PeerEvent::PeersUpdated { peers, self_ip });
        thread::sleep(DISCOVERY_INTERVAL);
    });

    rx
}

fn handle_incoming(
    stream: TcpStream,
    my_name: &str,
    tx: &mpsc::Sender<PeerEvent>,
    shares: &ShareRegistry,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let msg: Wire = serde_json::from_str(line.trim())?;

    match msg {
        Wire::Hello { .. } => {
            let resp = Wire::Hello { user: my_name.to_string() };
            writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
            writer.flush()?;
        }
        Wire::Message { from, text } => {
            writeln!(writer, "{}", serde_json::to_string(&Wire::Ack)?)?;
            writer.flush()?;
            let _ = tx.send(PeerEvent::IncomingMessage { from, text });
        }
        Wire::ShareOffer { from, title, host, token, writable } => {
            writeln!(writer, "{}", serde_json::to_string(&Wire::Ack)?)?;
            writer.flush()?;
            let _ = tx.send(PeerEvent::ShareOffer { from, title, host, token, writable });
        }
        Wire::JoinShare { token, user } => {
            // Token is the capability: only explicitly shared panes are
            // reachable, only while the share is active.
            let entry = shares.lock().ok().and_then(|m| m.get(&token).cloned());
            match entry {
                // The reader rides along: viewer Input lines may already sit
                // in its buffer, so the stream loop must reuse it, not re-clone.
                Some(entry) => run_share_stream(writer, reader, entry, token, user, tx),
                None => {
                    let err = Wire::ShareError {
                        message: "invalid or expired share token".into(),
                    };
                    writeln!(writer, "{}", serde_json::to_string(&err)?)?;
                    writer.flush()?;
                }
            }
        }
        // Stream-only frames arriving as a fresh request are protocol noise.
        Wire::Ack
        | Wire::ShareAccepted { .. }
        | Wire::PaneFrame { .. }
        | Wire::ShareEnd { .. }
        | Wire::ShareError { .. }
        | Wire::Input { .. }
        | Wire::WriteStatus { .. } => {}
    }
    Ok(())
}

/// Per-viewer streaming loop, run on the JoinShare connection's own thread.
/// Sends ShareAccepted, then a PaneFrame whenever the screen hash changes
/// (~10 fps idle, ~30 fps briefly after remote input), then ShareEnd when the
/// pane dies or the share is stopped. A companion reader thread drains viewer
/// Input lines from the same socket — they only ever become ViewerInput
/// events for main.rs to sanitize and route; peer threads never touch a PTY.
/// While the share is not writable, viewer input is dropped here.
fn run_share_stream(
    mut writer: TcpStream,
    reader: BufReader<TcpStream>,
    entry: ShareEntry,
    token: String,
    viewer: String,
    tx: &mpsc::Sender<PeerEvent>,
) {
    writer.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let send = |w: &mut TcpStream, msg: &Wire| -> Result<()> {
        writeln!(w, "{}", serde_json::to_string(msg)?)?;
        w.flush()?;
        Ok(())
    };

    let Some((rows, cols, _)) = (entry.capture)() else {
        let _ = send(&mut writer, &Wire::ShareEnd { reason: "pane closed".into() });
        return;
    };
    let mut last_writable = entry.writable.load(Ordering::Relaxed);
    if send(
        &mut writer,
        &Wire::ShareAccepted {
            title: entry.title.clone(),
            rows,
            cols,
            writable: last_writable,
        },
    )
    .is_err()
    {
        return;
    }

    if let Ok(mut v) = entry.viewers.lock() {
        v.push(viewer.clone());
    }
    let _ = tx.send(PeerEvent::ViewerJoined {
        viewer: viewer.clone(),
        title: entry.title.clone(),
    });

    // Set true by the input thread when the viewer's socket closes, so the
    // write loop tears down promptly even on an idle pane (it otherwise only
    // notices a departed viewer on its next failed frame send).
    let gone = Arc::new(AtomicBool::new(false));

    // Input reader: blocks on the socket; exits on EOF/error. The write loop
    // below shuts the socket down when the share ends, which unblocks us.
    let input_thread = {
        let mut reader = reader;
        let writable = entry.writable.clone();
        let last_input = entry.last_input.clone();
        let gone = gone.clone();
        let tx = tx.clone();
        let token = token.clone();
        let viewer = viewer.clone();
        thread::spawn(move || {
            // handle_incoming set a 5s handshake timeout — the stream is
            // long-lived and quiet from here, so block indefinitely.
            reader.get_ref().set_read_timeout(None).ok();
            let mut line = String::new();
            let mut win_start = std::time::Instant::now();
            let mut win_used = 0usize;
            loop {
                line.clear();
                // Bound the read itself — a line over the cap (or with no
                // newline) means an oversized/hostile message: drop the peer.
                let n = {
                    let mut limited = (&mut reader).take(MAX_INPUT_LINE as u64 + 1);
                    limited.read_line(&mut line)
                };
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(_) if !line.ends_with('\n') => break,
                    Ok(_) => {}
                }
                let Ok(Wire::Input { data }) = serde_json::from_str::<Wire>(line.trim()) else {
                    continue; // unknown/other lines — skip
                };
                if !writable.load(Ordering::Relaxed) {
                    continue; // read-only (or revoked) — input is dropped here
                }
                // Coarse rate cap at ingest so a flood never enters the channel.
                let now = std::time::Instant::now();
                if now.duration_since(win_start) > Duration::from_secs(1) {
                    win_start = now;
                    win_used = 0;
                }
                if win_used + data.len() > MAX_INPUT_BYTES_PER_SEC {
                    continue;
                }
                win_used += data.len();
                if let Ok(mut t) = last_input.lock() {
                    *t = Some(now);
                }
                let _ = tx.send(PeerEvent::ViewerInput {
                    token: token.clone(),
                    viewer: viewer.clone(),
                    data,
                });
            }
            gone.store(true, Ordering::Relaxed);
        })
    };

    let mut last_hash: Option<u64> = None;
    loop {
        if entry.stop.load(Ordering::Relaxed) {
            let _ = send(&mut writer, &Wire::ShareEnd { reason: "share stopped".into() });
            break;
        }
        if gone.load(Ordering::Relaxed) {
            break; // viewer's socket closed — the input thread saw it first
        }
        let writable = entry.writable.load(Ordering::Relaxed);
        if writable != last_writable {
            last_writable = writable;
            if send(&mut writer, &Wire::WriteStatus { writable }).is_err() {
                break;
            }
        }
        let Some((rows, cols, seq)) = (entry.capture)() else {
            let _ = send(&mut writer, &Wire::ShareEnd { reason: "pane closed".into() });
            break;
        };
        let mut h = DefaultHasher::new();
        seq.hash(&mut h);
        let hash = h.finish();
        if last_hash != Some(hash) {
            last_hash = Some(hash);
            if send(&mut writer, &Wire::PaneFrame { rows, cols, seq }).is_err() {
                break; // viewer went away — drop silently
            }
        }
        let hot = entry
            .last_input
            .lock()
            .ok()
            .and_then(|t| *t)
            .is_some_and(|t| t.elapsed() < INPUT_HOT_WINDOW);
        thread::sleep(if hot { FRAME_INTERVAL_HOT } else { FRAME_INTERVAL });
    }

    // Unblock the input reader's blocking read_line, then reap it.
    let _ = writer.shutdown(std::net::Shutdown::Both);
    let _ = input_thread.join();

    if let Ok(mut v) = entry.viewers.lock() {
        if let Some(i) = v.iter().position(|u| u == &viewer) {
            v.remove(i);
        }
    }
    let _ = tx.send(PeerEvent::ViewerLeft {
        viewer,
        title: entry.title,
    });
}

/// Offer a pane share to a peer. One-shot connection, like send_message.
pub fn send_share_offer(
    peer_ip: &str,
    from: &str,
    title: &str,
    host: &str,
    token: &str,
    writable: bool,
) -> Result<()> {
    let addr: SocketAddr = format!("{peer_ip}:{}", peemux_port()).parse()?;
    let stream = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT)?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let msg = Wire::ShareOffer {
        from: from.to_string(),
        title: title.to_string(),
        host: host.to_string(),
        token: token.to_string(),
        writable,
    };
    writeln!(writer, "{}", serde_json::to_string(&msg)?)?;
    writer.flush()?;

    let mut ack = String::new();
    reader.read_line(&mut ack)?;
    Ok(())
}

/// A successfully joined share: the handshake is done and `reader` is the
/// live frame stream (ShareAccepted consumed; PaneFrames/ShareEnd follow).
/// The reader must be kept — frames may already sit in its buffer. `writer`
/// is the same socket's write half, kept for Input when the share is writable
/// (write access can also be granted later via WriteStatus).
pub struct JoinedShare {
    pub reader: BufReader<TcpStream>,
    pub writer: TcpStream,
    pub title: String,
    pub rows: u16,
    pub cols: u16,
    pub writable: bool,
}

/// Connect to a sharer and join a share. Blocking (bounded by the connect +
/// handshake timeouts); on success the socket stays open as the frame stream.
pub fn join_share(host: &str, token: &str, user: &str) -> Result<JoinedShare> {
    let addr: SocketAddr = format!("{host}:{}", peemux_port())
        .parse()
        .map_err(|e| anyhow!("bad share host {host}: {e}"))?;
    let stream = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT)?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok();

    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let msg = Wire::JoinShare { token: token.to_string(), user: user.to_string() };
    writeln!(writer, "{}", serde_json::to_string(&msg)?)?;
    writer.flush()?;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    match serde_json::from_str::<Wire>(line.trim()) {
        Ok(Wire::ShareAccepted { title, rows, cols, writable }) => {
            // Handshake done — the stream is long-lived from here. Frames can
            // be sparse (only on change), so drop the read timeout; pane kill
            // shuts the socket down to unblock the reader thread. Bound WRITES
            // though: send_input runs on the TUI main thread, so a wedged
            // sharer must not be able to freeze the viewer's whole UI on a
            // full send buffer — a timed-out write just drops the keystroke.
            reader.get_ref().set_read_timeout(None).ok();
            reader.get_ref().set_write_timeout(Some(Duration::from_secs(5))).ok();
            Ok(JoinedShare { reader, writer, title, rows, cols, writable })
        }
        Ok(Wire::ShareError { message }) => Err(anyhow!("share refused: {message}")),
        Ok(_) => Err(anyhow!("unexpected reply from sharer")),
        Err(e) => Err(anyhow!("bad reply from sharer: {e}")),
    }
}

/// Read one Wire line from a joined share's frame stream. Returns the raw
/// enum decoded into the small public event the pane reader thread consumes.
pub enum StreamEvent {
    Frame { rows: u16, cols: u16, seq: String },
    End { reason: String },
    WriteStatus { writable: bool },
}

/// Blocking read of the next stream event. Ok(None) = unknown line (skip).
/// Err = socket closed / share over.
pub fn read_stream_event(reader: &mut BufReader<TcpStream>) -> Result<Option<StreamEvent>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(anyhow!("stream closed"));
    }
    match serde_json::from_str::<Wire>(line.trim()) {
        Ok(Wire::PaneFrame { rows, cols, seq }) => Ok(Some(StreamEvent::Frame { rows, cols, seq })),
        Ok(Wire::ShareEnd { reason }) => Ok(Some(StreamEvent::End { reason })),
        Ok(Wire::ShareError { message }) => Ok(Some(StreamEvent::End { reason: message })),
        Ok(Wire::WriteStatus { writable }) => Ok(Some(StreamEvent::WriteStatus { writable })),
        _ => Ok(None),
    }
}

/// Viewer → sharer: send keystrokes up a writable share's stream socket.
/// Called from the TUI main thread only; one whole message per call so a
/// key's escape sequence can never interleave with another writer's bytes.
pub fn send_input(sock: &TcpStream, data: &str) -> Result<()> {
    let mut w = sock;
    writeln!(w, "{}", serde_json::to_string(&Wire::Input { data: data.to_string() })?)?;
    w.flush()?;
    Ok(())
}

/// Send a message to a peer. Blocking — call from a background thread.
pub fn send_message(peer_ip: &str, from: &str, text: &str) -> Result<()> {
    let addr: SocketAddr = format!("{peer_ip}:{}", peemux_port()).parse()?;
    let stream = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT)?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let msg = Wire::Message {
        from: from.to_string(),
        text: text.to_string(),
    };
    writeln!(writer, "{}", serde_json::to_string(&msg)?)?;
    writer.flush()?;

    let mut ack = String::new();
    reader.read_line(&mut ack)?;
    Ok(())
}

// ─── tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Wire) {
        let json = serde_json::to_string(&msg).unwrap();
        let back: Wire = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back, "roundtrip mismatch for {json}");
    }

    #[test]
    fn wire_share_variants_roundtrip() {
        roundtrip(Wire::ShareOffer {
            from: "tyler".into(),
            title: "fix-tests".into(),
            host: "100.64.0.1".into(),
            token: "Ab3xK9Ab3xK9Ab3xK9Ab3xK9".into(),
            writable: true,
        });
        roundtrip(Wire::JoinShare {
            token: "Ab3xK9Ab3xK9Ab3xK9Ab3xK9".into(),
            user: "alice".into(),
        });
        roundtrip(Wire::ShareAccepted {
            title: "fix-tests".into(),
            rows: 50,
            cols: 140,
            writable: false,
        });
        roundtrip(Wire::PaneFrame {
            rows: 2,
            cols: 5,
            seq: "\x1b[2J\x1b[H\x1b[0;31mhi\r\nyo\x1b[0m".into(),
        });
        roundtrip(Wire::ShareEnd { reason: "pane closed".into() });
        roundtrip(Wire::ShareError { message: "invalid or expired share token".into() });
        roundtrip(Wire::Input { data: "ls -la\r".into() });
        roundtrip(Wire::Input { data: "\x1b[A\x03".into() }); // arrows + ctrl-c survive JSON
        roundtrip(Wire::WriteStatus { writable: true });
    }

    #[test]
    fn old_sharer_messages_default_to_read_only() {
        // A pre-write-access sharer omits `writable` — must parse as false,
        // never fail, so mixed-version shares stay read-only-safe.
        let accepted: Wire = serde_json::from_str(
            r#"{"type":"shareaccepted","title":"t","rows":10,"cols":20}"#,
        )
        .unwrap();
        assert!(matches!(accepted, Wire::ShareAccepted { writable: false, .. }));
        let offer: Wire = serde_json::from_str(
            r#"{"type":"shareoffer","from":"t","title":"x","host":"h","token":"k"}"#,
        )
        .unwrap();
        assert!(matches!(offer, Wire::ShareOffer { writable: false, .. }));
    }

    #[test]
    fn old_peers_drop_unknown_variants() {
        // A peer running the pre-share Wire enum must fail to parse the new
        // tags (and drop the line) rather than misparse them as something else.
        let json = r#"{"type":"shareoffer","from":"t","title":"x","host":"h","token":"k"}"#;
        let parsed: Wire = serde_json::from_str(json).unwrap();
        assert!(matches!(parsed, Wire::ShareOffer { .. }));
        // Unknown tag on OUR side degrades the same way: hard parse error.
        assert!(serde_json::from_str::<Wire>(r#"{"type":"future-thing"}"#).is_err());
    }

    #[test]
    fn random_token_shape() {
        let t1 = random_token();
        let t2 = random_token();
        assert_eq!(t1.len(), 24);
        assert!(t1.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(t1, t2);
    }

    /// End-to-end streaming loop against a scripted TCP client: JoinShare in,
    /// ShareAccepted + initial PaneFrame out, ShareEnd on stop.
    #[test]
    fn join_share_streams_frames_then_ends() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let entry = ShareEntry {
            title: "fix-tests".into(),
            capture: Arc::new(|| Some((3, 10, "\x1b[2J\x1b[Hhello".to_string()))),
            stop: stop.clone(),
            writable: Arc::new(AtomicBool::new(false)),
            last_input: Arc::new(Mutex::new(None)),
            viewers: Arc::new(Mutex::new(Vec::new())),
        };
        let registry: ShareRegistry = Arc::new(Mutex::new(HashMap::new()));
        registry.lock().unwrap().insert("tok123".into(), entry);

        let (tx, _rx) = mpsc::channel();
        let reg_srv = registry.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = handle_incoming(stream, "sharer", &tx, &reg_srv);
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);

        let join = Wire::JoinShare { token: "tok123".into(), user: "alice".into() };
        writeln!(writer, "{}", serde_json::to_string(&join).unwrap()).unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<Wire>(line.trim()).unwrap(),
            Wire::ShareAccepted { rows: 3, cols: 10, .. }
        ));

        line.clear();
        reader.read_line(&mut line).unwrap();
        match serde_json::from_str::<Wire>(line.trim()).unwrap() {
            Wire::PaneFrame { rows, cols, seq } => {
                assert_eq!((rows, cols), (3, 10));
                assert_eq!(seq, "\x1b[2J\x1b[Hhello");
            }
            other => panic!("expected PaneFrame, got {other:?}"),
        }

        // Capture is static → no more frames. Stop the share; expect ShareEnd.
        stop.store(true, Ordering::Relaxed);
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<Wire>(line.trim()).unwrap(),
            Wire::ShareEnd { .. }
        ));
        server.join().unwrap();
    }

    #[test]
    fn join_share_bad_token_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let registry: ShareRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = handle_incoming(stream, "sharer", &tx, &registry);
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let join = Wire::JoinShare { token: "nope".into(), user: "mallory".into() };
        writeln!(writer, "{}", serde_json::to_string(&join).unwrap()).unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<Wire>(line.trim()).unwrap(),
            Wire::ShareError { .. }
        ));
        server.join().unwrap();
    }

    /// Write path end-to-end: viewer Input on a writable share surfaces as
    /// ViewerInput; revoking write streams WriteStatus{false} to the viewer
    /// and drops subsequent input; input while read-only never surfaces.
    #[test]
    fn writable_share_routes_input_and_revokes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let writable = Arc::new(AtomicBool::new(true));
        let entry = ShareEntry {
            title: "pair".into(),
            capture: Arc::new(|| Some((3, 10, "\x1b[2J\x1b[Hhello".to_string()))),
            stop: stop.clone(),
            writable: writable.clone(),
            last_input: Arc::new(Mutex::new(None)),
            viewers: Arc::new(Mutex::new(Vec::new())),
        };
        let registry: ShareRegistry = Arc::new(Mutex::new(HashMap::new()));
        registry.lock().unwrap().insert("tokw".into(), entry);

        let (tx, rx) = mpsc::channel();
        let reg_srv = registry.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = handle_incoming(stream, "sharer", &tx, &reg_srv);
        });

        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);

        let join = Wire::JoinShare { token: "tokw".into(), user: "alice".into() };
        writeln!(writer, "{}", serde_json::to_string(&join).unwrap()).unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<Wire>(line.trim()).unwrap(),
            Wire::ShareAccepted { writable: true, .. }
        ));

        // Viewer types — the sharer side must surface it as ViewerInput.
        let input = Wire::Input { data: "echo hi\r".into() };
        writeln!(writer, "{}", serde_json::to_string(&input).unwrap()).unwrap();
        let ev = rx
            .iter()
            .find(|e| matches!(e, PeerEvent::ViewerInput { .. }))
            .unwrap();
        match ev {
            PeerEvent::ViewerInput { token, viewer, data } => {
                assert_eq!(token, "tokw");
                assert_eq!(viewer, "alice");
                assert_eq!(data, "echo hi\r");
            }
            _ => unreachable!(),
        }

        // Revoke write — the viewer is told over the stream.
        writable.store(false, Ordering::Relaxed);
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            match serde_json::from_str::<Wire>(line.trim()).unwrap() {
                Wire::WriteStatus { writable } => {
                    assert!(!writable);
                    break;
                }
                Wire::PaneFrame { .. } => continue, // initial frame may precede it
                other => panic!("expected WriteStatus, got {other:?}"),
            }
        }

        // Input after revoke is dropped on the sharer side.
        let input = Wire::Input { data: "rm -rf /\r".into() };
        writeln!(writer, "{}", serde_json::to_string(&input).unwrap()).unwrap();
        thread::sleep(Duration::from_millis(250));
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();
        let leaked = rx
            .try_iter()
            .any(|e| matches!(e, PeerEvent::ViewerInput { .. }));
        assert!(!leaked, "input after revoke must not surface");
    }
}
