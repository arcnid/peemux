// Tailscale peer discovery + direct peemux messaging.
//
// Each peemux instance listens on TCP 9867. A background thread polls
// `tailscale status --json` for online peers, then probes each on 9867.
// Peers that respond with a Hello handshake appear in the friends list.
// Messages flow directly over TCP — no external service needed.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

const PEEMUX_PORT: u16 = 9867;
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

// ─── public types ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Peer {
    pub hostname: String,
    pub display_name: String,
    pub tailscale_ip: String,
    pub has_unread: bool,
}

pub enum PeerEvent {
    PeersUpdated(Vec<Peer>),
    IncomingMessage { from: String, text: String },
    Error(String),
}

// ─── wire protocol (newline-delimited JSON on TCP 9867) ───────────────────

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Wire {
    Hello { user: String },
    Message { from: String, text: String },
    Ack,
}

// ─── tailscale status JSON ────────────────────────────────────────────────

#[derive(Deserialize)]
struct TsStatus {
    #[serde(rename = "Peer")]
    peer: Option<HashMap<String, TsNode>>,
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

fn get_tailscale_peers() -> Vec<(String, String)> {
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
        _ => return vec![],
    };
    let status: TsStatus = match serde_json::from_slice(&output.stdout) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    status
        .peer
        .unwrap_or_default()
        .into_values()
        .filter(|n| n.online.unwrap_or(false))
        .filter_map(|n| {
            let hostname = n.host_name?;
            let ip = n.tailscale_ips?.into_iter().find(|ip| ip.starts_with("100."))?;
            Some((hostname, ip))
        })
        .collect()
}

fn probe_peemux(ip: &str, my_name: &str) -> Option<String> {
    let addr: SocketAddr = format!("{ip}:{PEEMUX_PORT}").parse().ok()?;
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

pub fn spawn_peer_system(display_name: String) -> mpsc::Receiver<PeerEvent> {
    let (tx, rx) = mpsc::channel();

    // Listener thread: accept incoming connections on PEEMUX_PORT.
    let tx_listen = tx.clone();
    let listen_name = display_name.clone();
    thread::spawn(move || {
        let listener = match TcpListener::bind(format!("0.0.0.0:{PEEMUX_PORT}")) {
            Ok(l) => l,
            Err(e) => {
                let _ = tx_listen.send(PeerEvent::Error(format!("bind :{PEEMUX_PORT}: {e}")));
                return;
            }
        };
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let tx = tx_listen.clone();
            let name = listen_name.clone();
            thread::spawn(move || {
                let _ = handle_incoming(stream, &name, &tx);
            });
        }
    });

    // Discovery thread: poll tailscale, probe peers.
    thread::spawn(move || loop {
        let ts_peers = get_tailscale_peers();
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
        let _ = tx.send(PeerEvent::PeersUpdated(peers));
        thread::sleep(DISCOVERY_INTERVAL);
    });

    rx
}

fn handle_incoming(
    stream: TcpStream,
    my_name: &str,
    tx: &mpsc::Sender<PeerEvent>,
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
        Wire::Ack => {}
    }
    Ok(())
}

/// Send a message to a peer. Blocking — call from a background thread.
pub fn send_message(peer_ip: &str, from: &str, text: &str) -> Result<()> {
    let addr: SocketAddr = format!("{peer_ip}:{PEEMUX_PORT}").parse()?;
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
