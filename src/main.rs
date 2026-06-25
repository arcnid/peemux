// peemux — the terminal for the agent era.
//
// M1: real PTYs.
// - Spawns with no windows. Welcome screen shows the PEEMUX logo + keybind cheat sheet.
// - Ctrl-B is the prefix (tmux-style). Visual indicator in the top bar when armed.
// - Ctrl-B c        → create a new window ($SHELL in a portable-pty)
// - Ctrl-B n / p    → next / previous window
// - Ctrl-B 0..9     → jump to window by index
// - Ctrl-B &        → kill current window
// - Ctrl-B ?        → toggle help overlay
// - Ctrl-B d        → quit peemux
// - Active window's PTY renders inside the body; keystrokes forward to the child.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Stdout, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, NamedColor, Processor as AnsiProcessor, Rgb,
};
use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use ratatui::{Frame, Terminal};

// ─── peemux palette ─────────────────────────────────────────────────────────
// C_BG is intentionally NOT painted as a base fill — host terminal's
// background (transparency, blur, image) shows through. Reserved for opaque
// overlays (help modal, splash) where contrast matters.
const C_BG: Color = Color::Rgb(0x1a, 0x00, 0x33);
const C_PANEL: Color = Color::Rgb(0x3a, 0x00, 0x66);
const C_PINK: Color = Color::Rgb(0xff, 0x00, 0xaa);
const C_FG: Color = Color::Rgb(0x88, 0xcc, 0xff);
const C_DIM: Color = Color::Rgb(0x66, 0x88, 0xaa);
const C_GREEN: Color = Color::Rgb(0x88, 0xff, 0x88);
const C_ORANGE: Color = Color::Rgb(0xff, 0xaa, 0x00);

// ─── logo ───────────────────────────────────────────────────────────────────
const LOGO: &[&str] = &[
    " ██████╗ ███████╗███████╗███╗   ███╗██╗   ██╗██╗  ██╗",
    " ██╔══██╗██╔════╝██╔════╝████╗ ████║██║   ██║╚██╗██╔╝",
    " ██████╔╝█████╗  █████╗  ██╔████╔██║██║   ██║ ╚███╔╝ ",
    " ██╔═══╝ ██╔══╝  ██╔══╝  ██║╚██╔╝██║██║   ██║ ██╔██╗ ",
    " ██║     ███████╗███████╗██║ ╚═╝ ██║╚██████╔╝██╔╝ ██╗",
    " ╚═╝     ╚══════╝╚══════╝╚═╝     ╚═╝ ╚═════╝ ╚═╝  ╚═╝",
];

const KEYBINDS: &[(&str, &str)] = &[
    ("Ctrl-b  c",      "create new window"),
    ("Ctrl-b  n / p",  "next / previous window"),
    ("Ctrl-b  0..9",   "jump to window N"),
    ("Ctrl-b  w",      "toggle wall ↔ single view"),
    ("Ctrl-b  Tab",    "toggle conductor sidebar"),
    ("Ctrl-b  o",      "focus sidebar ↔ workers"),
    ("Ctrl-b  ↑↓←→",  "move active pane (wall view)"),
    ("mouse click",    "focus the pane under the cursor"),
    ("mouse scroll",   "scroll the pane under the cursor"),
    ("Ctrl-b  x / &",  "force-kill current window"),
    ("exit / Ctrl-d",  "close window from inside the shell"),
    ("Ctrl-b  ?",      "toggle this help"),
    ("Ctrl-b  d",      "quit peemux"),
];

// ─── agent heuristic patterns ───────────────────────────────────────────────
// Cheap "did this agent print a known prompt?" string-contains checks. Blocked
// is matched before Working — a "Continue?" pane is still working but the
// user needs to see the prompt.
const BLOCKED_PATTERNS: &[&str] = &[
    "Continue?",
    "Do you want",
    "[Y/n]",
    "[y/N]",
    "(y/n)",
    "Press y",
    "Press Y",
    "❯ 1.",  // claude's interactive choice menu
];
const WORKING_PATTERNS: &[&str] = &[
    "(esc to interrupt)",
    "esc to interrupt",
    "Thinking",
    "Generating",
    "Loading…",
    "Processing",
];

// ─── conductor instructions (written to a tmpdir as CLAUDE.md before launch) ──
const CONDUCTOR_INSTRUCTIONS: &str = r#"# peemux conductor

You are the conductor for peemux, the user's terminal multiplexer. Your job is
to orchestrate worker panes on the user's behalf — the user talks to YOU, and
you drive the workers.

You drive peemux via your Bash tool using these commands:

- `peemux ls` — list panes: id, title, alive, agent, state, rows×cols
- `peemux spawn [cmd]` — open a new pane running cmd (default: claude). Prints pane-id.
- `peemux send-keys <id> <text>` — type text into a pane AND PRESS ENTER. This
  is the default. The Enter is auto-appended so the prompt actually submits.
- `peemux send-keys <id> <text> --raw` — type without pressing Enter. Use this
  only when you specifically want to leave the input unsent (e.g. for control
  bytes or partial typing). Almost never what you want.
- `peemux capture-pane <id>` — dump the visible screen of a pane as plain text.
  Works on ANY pane — agent or plain shell — so you can read what the user is
  doing in their own zsh / nvim / nmap / tail windows for context.
- `peemux kill-pane <id>` — close a pane.
- `peemux notify <source> <title> <body>` — send a notification toast + voice.
- `peemux agent state <id> <state>` — explicitly push state (blocked|working|
  done|idle) for a pane. Useful when you know more than the heuristic does.

Critical: send-keys auto-submits by default. Do NOT manually chase it with a
follow-up Enter — that creates a double-submit. If your first send-keys didn't
appear to submit, the worker may have been mid-startup; wait 1–2s and inspect
with capture-pane before resending.

Examples:

- User: "spin up two claudes to work on tests and docs"
  1. `peemux spawn claude` → returns id, e.g. `1`
  2. `peemux spawn claude` → returns id, e.g. `2`
  3. `peemux send-keys 1 "fix failing tests in src/"`
  4. `peemux send-keys 2 "update README with new install steps"`

- User: "what's worker 3 doing?"
  1. `peemux capture-pane 3` → read its current screen, summarize back.

- User has a shell pane running `tail -f app.log`:
  - You can `peemux capture-pane <id>` on it to read the latest log lines
    when deciding what to brief an agent with.

The user's cockpit is heterogeneous: some panes are agents you spawned, some
are the user's own shells (nvim, nmap, logs, etc.). You can see ALL of them via
capture-pane — use that context. But only send-keys to the panes the user
asked you to drive; don't type into the user's own shells unprompted.

Keep responses short. The user can see the worker panes themselves — don't
mirror their output. Focus on coordinating, briefing, and reporting state.
"#;

// ─── CLI ────────────────────────────────────────────────────────────────────
#[derive(Parser, Debug)]
#[command(name = "peemux", version, about = "The terminal for the agent era")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the TUI client (default when no subcommand is given).
    Tui,
    /// List all panes (id, title, alive, rows×cols).
    Ls,
    /// Open a new pane and launch a command (default: claude). Prints pane-id.
    Spawn {
        #[arg(default_value = "claude")]
        cmd: String,
    },
    /// Type text into a pane and submit (Enter is auto-appended). Use `--raw`
    /// to send literal bytes without appending a return.
    #[command(name = "send-keys")]
    SendKeys {
        id: u64,
        text: String,
        /// Send text literally — do NOT append a return. Use for control
        /// bytes, mid-prompt typing, or any case where you want to leave
        /// the input unsent.
        #[arg(long)]
        raw: bool,
    },
    /// Dump the visible screen of a pane as plain text.
    #[command(name = "capture-pane")]
    CapturePane { id: u64 },
    /// Close a pane (SIGKILLs the child).
    #[command(name = "kill-pane")]
    KillPane { id: u64 },
    /// Send a notification toast to the running TUI.
    Notify { source: String, title: String, body: String },
    /// Stop the running peemux server.
    #[command(name = "kill-server")]
    KillServer,
    /// Push agent state or list all attached agents.
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
}

#[derive(Subcommand, Debug)]
enum AgentAction {
    /// Push an agent state for a pane.
    State { pane_id: u64, state: String },
    /// List all panes with attached agents and their states.
    List,
}

/// Per-pane agent state — the 4-state badge the sidebar dots render. `None`
/// means "no agent attached" (regular shell window).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum AgentState {
    None,
    /// 🔴 needs your input
    Blocked,
    /// 🟡 making progress
    Working,
    /// 🔵 finished, you haven't focused it since
    Done,
    /// 🟢 finished, acknowledged
    Idle,
}

impl AgentState {
    fn dot(self) -> &'static str {
        match self {
            AgentState::None => "",
            AgentState::Blocked => "●",
            AgentState::Working => "●",
            AgentState::Done => "●",
            AgentState::Idle => "●",
        }
    }
    fn color(self) -> Color {
        match self {
            AgentState::None => C_DIM,
            AgentState::Blocked => Color::Rgb(0xff, 0x55, 0x55),
            AgentState::Working => Color::Rgb(0xff, 0xcc, 0x33),
            AgentState::Done => Color::Rgb(0x55, 0xaa, 0xff),
            AgentState::Idle => Color::Rgb(0x66, 0xdd, 0x77),
        }
    }
    fn label(self) -> &'static str {
        match self {
            AgentState::None => "none",
            AgentState::Blocked => "blocked",
            AgentState::Working => "working",
            AgentState::Done => "done",
            AgentState::Idle => "idle",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Some(AgentState::None),
            "blocked" => Some(AgentState::Blocked),
            "working" => Some(AgentState::Working),
            "done" => Some(AgentState::Done),
            "idle" => Some(AgentState::Idle),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct AgentInfo {
    id: u64,
    title: String,
    agent: Option<String>,
    state: AgentState,
}

// ─── entry ──────────────────────────────────────────────────────────────────
fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None | Some(Command::Tui) => run_tui(),
        Some(Command::Ls) => client_run(Request::Ls),
        Some(Command::Spawn { cmd }) => client_run(Request::Spawn { cmd: Some(cmd) }),
        Some(Command::SendKeys { id, text, raw }) => {
            // Default = submit. The overwhelming common case (conductor →
            // worker, conductor → shell) wants the keystrokes to land AND
            // press Enter. Pass --raw if you specifically want to leave the
            // text un-sent (e.g. typing a partial prompt, sending a control
            // byte, or pasting multi-line input you'll submit later).
            let t = if raw { text } else { format!("{text}\r") };
            client_run(Request::SendKeys { id, text: t })
        }
        Some(Command::CapturePane { id }) => client_run(Request::CapturePane { id }),
        Some(Command::KillPane { id }) => client_run(Request::KillPane { id }),
        Some(Command::Notify { source, title, body }) => {
            client_run(Request::Notify { source, title, body })
        }
        Some(Command::KillServer) => client_run(Request::KillServer),
        Some(Command::Agent { action }) => match action {
            AgentAction::State { pane_id, state } => match AgentState::parse(&state) {
                Some(s) => client_run(Request::AgentState { id: pane_id, state: s }),
                None => Err(anyhow!(
                    "unknown state '{state}' (expected: none|blocked|working|done|idle)"
                )),
            },
            AgentAction::List => client_run(Request::AgentList),
        },
    }
}

// ─── IPC protocol (UDS, newline-delimited JSON) ────────────────────────────
//
// peemux server runs in-process with the TUI: while a `peemux` TUI is
// attached, it listens on a unix domain socket at /tmp/peemux-$USER.sock
// and accepts one request per connection. Other invocations of the binary
// (`peemux spawn`, `peemux ls`, etc.) connect to the socket, write one
// Request JSON + newline, read one Response JSON + newline, and exit. The
// conductor Claude uses these via its Bash tool to drive peemux without
// needing any API/SDK.

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum Request {
    Ls,
    Spawn { cmd: Option<String> },
    SendKeys { id: u64, text: String },
    CapturePane { id: u64 },
    KillPane { id: u64 },
    Notify { source: String, title: String, body: String },
    KillServer,
    AgentState { id: u64, state: AgentState },
    AgentList,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum Response {
    Ok,
    Ls { panes: Vec<PaneInfo> },
    Spawned { id: u64, title: String },
    Captured { text: String },
    Agents { agents: Vec<AgentInfo> },
    Error { message: String },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct PaneInfo {
    id: u64,
    title: String,
    alive: bool,
    rows: u16,
    cols: u16,
    agent: Option<String>,
    state: AgentState,
}

fn socket_path() -> PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
    PathBuf::from(format!("/tmp/peemux-{user}.sock"))
}

/// Send one Request, print/echo the Response. Used by the CLI subcommands.
fn client_run(req: Request) -> Result<()> {
    let sock = socket_path();
    let mut stream = UnixStream::connect(&sock).map_err(|e| {
        anyhow!(
            "no peemux server at {} — start one with `peemux` ({e})",
            sock.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let line = serde_json::to_string(&req)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    let resp: Response = serde_json::from_str(buf.trim())
        .map_err(|e| anyhow!("server returned malformed response: {e}\n  raw: {buf}"))?;
    print_response(&req, &resp);
    Ok(())
}

fn print_response(req: &Request, resp: &Response) {
    match (req, resp) {
        (_, Response::Error { message }) => {
            eprintln!("error: {message}");
        }
        (Request::Ls, Response::Ls { panes }) => {
            if panes.is_empty() {
                println!("(no panes)");
            } else {
                for p in panes {
                    let mark = if p.alive { " " } else { "✗" };
                    let agent = p.agent.clone().unwrap_or_else(|| "-".into());
                    println!(
                        "{:>4}  {}  {:<20}  {:<8}  {:<8}  {}x{}",
                        p.id,
                        mark,
                        p.title,
                        agent,
                        p.state.label(),
                        p.cols,
                        p.rows
                    );
                }
            }
        }
        (Request::AgentList, Response::Agents { agents }) => {
            if agents.is_empty() {
                println!("(no panes)");
            } else {
                for a in agents {
                    let agent = a.agent.clone().unwrap_or_else(|| "-".into());
                    println!(
                        "{:>4}  {:<20}  {:<8}  {}",
                        a.id, a.title, agent, a.state.label()
                    );
                }
            }
        }
        (Request::Spawn { .. }, Response::Spawned { id, title }) => {
            println!("{id}\t{title}");
        }
        (Request::CapturePane { .. }, Response::Captured { text }) => {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
        }
        (_, Response::Ok) => {
            // Quiet by design — exit 0 is the signal.
        }
        _ => {
            // Mismatched response shape — print raw JSON so the user sees it.
            if let Ok(j) = serde_json::to_string(resp) {
                println!("{j}");
            }
        }
    }
}

/// Server-side: bridges a request from a connection thread into the TUI's
/// main thread, awaits a response, and ships it back to the client.
struct ServerRequest {
    req: Request,
    reply: mpsc::Sender<Response>,
}

/// Drop-guard that removes the socket file when the TUI exits.
struct SocketCleanup(PathBuf);
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Spawn the UDS listener. Each accepted connection gets its own thread that
/// reads one Request, sends it over `req_tx` to the TUI main thread, waits
/// up to 5s for a Response, and writes it back to the client.
fn spawn_server(req_tx: mpsc::Sender<ServerRequest>) -> Result<SocketCleanup> {
    let path = socket_path();
    // Stale socket from a previous crashed TUI — clean it.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .map_err(|e| anyhow!("bind {}: {e}", path.display()))?;
    let cleanup = SocketCleanup(path.clone());

    thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(stream) = incoming else { continue };
            let req_tx = req_tx.clone();
            thread::spawn(move || {
                let _ = handle_client(stream, req_tx);
            });
        }
    });

    Ok(cleanup)
}

fn handle_client(stream: UnixStream, req_tx: mpsc::Sender<ServerRequest>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            let resp = Response::Error { message: format!("bad request: {e}") };
            let _ = writeln!(writer, "{}", serde_json::to_string(&resp)?);
            return Ok(());
        }
    };
    let (reply_tx, reply_rx) = mpsc::channel();
    if req_tx.send(ServerRequest { req, reply: reply_tx }).is_err() {
        let resp = Response::Error { message: "server shutting down".into() };
        let _ = writeln!(writer, "{}", serde_json::to_string(&resp)?);
        return Ok(());
    }
    let resp = reply_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or(Response::Error { message: "server timeout".into() });
    writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
    Ok(())
}

// ─── window (one PTY + parser + child) ──────────────────────────────────────
/// State the reader thread feeds and the render thread reads. Holding the
/// VT processor + terminal state behind one mutex keeps them in lockstep —
/// alacritty's parser is stateful per terminal.
struct VtState {
    parser: AnsiProcessor,
    term: Term<VoidListener>,
}

struct Window {
    id: u64,
    title: String,
    vt: Arc<Mutex<VtState>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    rows: u16,
    cols: u16,
    alive: bool,
    /// Detected or explicitly-set agent in this pane (e.g. "claude", "codex",
    /// "aider"). None when it's a plain shell window.
    agent: Option<String>,
    state: AgentState,
}

impl Window {
    fn new(id: u64, cmd: CommandBuilder, rows: u16, cols: u16, dirty: Arc<AtomicBool>) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| anyhow!("openpty: {e}"))?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow!("spawn: {e}"))?;
        // Close the slave handle in the parent so EOF propagates when the
        // child exits.
        drop(pair.slave);

        let config = TermConfig {
            scrolling_history: 5_000,
            ..TermConfig::default()
        };
        let size = TermSize::new(cols as usize, rows as usize);
        let term = Term::new(config, &size, VoidListener);
        let parser = AnsiProcessor::new();
        let vt = Arc::new(Mutex::new(VtState { parser, term }));

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow!("clone_reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow!("take_writer: {e}"))?;

        let vt_thread = vt.clone();
        let dirty_thread = dirty.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut state) = vt_thread.lock() {
                            // Borrow split: split fields so parser can take
                            // &mut term as its Handler in one call.
                            let VtState { parser, term } = &mut *state;
                            parser.advance(term, &buf[..n]);
                        }
                        dirty_thread.store(true, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            }
            dirty_thread.store(true, Ordering::Relaxed);
        });

        Ok(Self {
            id,
            title: format!("win-{id}"),
            vt,
            master: pair.master,
            writer,
            child,
            rows,
            cols,
            alive: true,
            agent: None,
            state: AgentState::None,
        })
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        if let Ok(mut state) = self.vt.lock() {
            state.term.resize(TermSize::new(cols as usize, rows as usize));
        }
    }

    fn scroll(&mut self, lines: i32) {
        if let Ok(mut state) = self.vt.lock() {
            state.term.scroll_display(Scroll::Delta(lines));
        }
    }

    fn poll_alive(&mut self) {
        if self.alive {
            if let Ok(Some(_)) = self.child.try_wait() {
                self.alive = false;
            }
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        self.alive = false;
    }
}

// ─── app state ──────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Single,
    Wall,
}

/// Where keystrokes go. Conductor focus is only meaningful while the sidebar
/// is visible AND the conductor PTY is alive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Workers,
    Conductor,
}

#[allow(dead_code)] // fields render in the M3 sidebar toast banner
#[derive(Clone, Debug)]
struct Toast {
    source: String,
    title: String,
    body: String,
    ts: u64, // unix seconds, for "5s ago" rendering later
}

struct App {
    windows: Vec<Window>,
    active: Option<usize>,
    next_id: u64,
    prefix_armed: bool,
    show_help: bool,
    last_msg: Option<String>,
    dirty: Arc<AtomicBool>,
    quit: bool,
    view: View,
    /// Last known PTY body size — used when the server spawns a window in
    /// response to `peemux spawn` and there's no fresh terminal.size() call.
    last_body: (u16, u16),
    /// Notification queue. M3 sidebar renders this as the toast banner.
    /// Capped at 50 — oldest dropped when full.
    toasts: VecDeque<Toast>,
    /// Conductor sidebar visibility. Default off — turning it on the first
    /// time auto-spawns the conductor PTY.
    sidebar_visible: bool,
    sidebar_width: u16,
    /// Pinned conductor PTY (a `claude` running with a generated CLAUDE.md
    /// that teaches it the peemux CLI). Lives outside `windows` so it doesn't
    /// show up in tab strips, wall view, or `peemux ls`.
    conductor: Option<Window>,
    /// Latched after the first spawn attempt — prevents tight-loop respawns
    /// when `claude` is missing on PATH. Reset on sidebar close.
    conductor_attempted: bool,
    focus: Focus,
    /// Frame counter used to throttle agent-state heuristics.
    tick: u64,
}

impl App {
    fn new() -> Self {
        Self {
            windows: Vec::new(),
            active: None,
            next_id: 1,
            prefix_armed: false,
            show_help: true, // welcome screen shows help by default
            last_msg: None,
            dirty: Arc::new(AtomicBool::new(true)),
            quit: false,
            view: View::Single,
            last_body: (80, 24),
            toasts: VecDeque::new(),
            sidebar_visible: false,
            sidebar_width: 44,
            conductor: None,
            conductor_attempted: false,
            focus: Focus::Workers,
            tick: 0,
        }
    }

    /// Open / close the conductor sidebar. First open lazily spawns the
    /// conductor PTY. Closing leaves the conductor running in the background.
    fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        if self.sidebar_visible {
            // Default to focusing the conductor when it appears — it's the
            // user's primary point of contact.
            if self.conductor_alive() {
                self.focus = Focus::Conductor;
            }
            self.last_msg = Some("sidebar on".into());
        } else {
            self.focus = Focus::Workers;
            // Reset spawn latch so re-opening can retry if it failed before.
            self.conductor_attempted = false;
            self.last_msg = Some("sidebar off".into());
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn toggle_focus(&mut self) {
        if !self.sidebar_visible {
            self.last_msg = Some("sidebar is closed (Ctrl-b Tab)".into());
            return;
        }
        self.focus = match self.focus {
            Focus::Workers => Focus::Conductor,
            Focus::Conductor => Focus::Workers,
        };
        self.last_msg = Some(
            match self.focus {
                Focus::Workers => "focus → workers",
                Focus::Conductor => "focus → conductor",
            }
            .into(),
        );
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Cheap text-pattern scan over each agent pane's visible screen. Runs
    /// every ~1s from the main loop (not every frame). Only touches panes
    /// with an attached agent (`w.agent.is_some()`); plain shell windows
    /// keep AgentState::None forever.
    fn run_heuristics(&mut self) {
        for w in self.windows.iter_mut() {
            if w.agent.is_none() || !w.alive {
                continue;
            }
            let text = capture_pane_text(w);
            let new_state = if BLOCKED_PATTERNS.iter().any(|p| text.contains(p)) {
                AgentState::Blocked
            } else if WORKING_PATTERNS.iter().any(|p| text.contains(p)) {
                AgentState::Working
            } else {
                // Transition Working → Done so the user sees the agent
                // finished; Done stays Done until they focus it.
                match w.state {
                    AgentState::Working => AgentState::Done,
                    AgentState::None => AgentState::Idle,
                    other => other,
                }
            };
            if new_state != w.state {
                w.state = new_state;
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }

    /// "Acknowledge" the active worker: Done → Idle the moment the user
    /// focuses or clicks it. Other states unchanged.
    fn ack_active(&mut self) {
        if let Some(i) = self.active {
            if let Some(w) = self.windows.get_mut(i) {
                if w.state == AgentState::Done {
                    w.state = AgentState::Idle;
                    self.dirty.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    fn conductor_alive(&self) -> bool {
        matches!(&self.conductor, Some(w) if w.alive)
    }

    /// Spawn the conductor PTY if it isn't already running. Called lazily on
    /// the first render after the sidebar becomes visible.
    fn ensure_conductor(&mut self, rows: u16, cols: u16) {
        if self.conductor_alive() || self.conductor_attempted || rows == 0 || cols == 0 {
            return;
        }
        self.conductor_attempted = true;
        // Generate a tmpdir with CLAUDE.md so the conductor's claude session
        // picks up the peemux CLI vocabulary as its project instructions.
        let tmp = std::env::temp_dir().join(format!("peemux-conductor-{}", std::process::id()));
        if std::fs::create_dir_all(&tmp).is_err() {
            self.last_msg = Some("conductor: tmpdir failed".into());
            return;
        }
        let _ = std::fs::write(tmp.join("CLAUDE.md"), CONDUCTOR_INSTRUCTIONS);

        let mut cmd = CommandBuilder::new("claude");
        cmd.env("TERM", "xterm-256color");
        cmd.cwd(&tmp);

        match Window::new(0, cmd, rows, cols, self.dirty.clone()) {
            Ok(mut w) => {
                w.title = "conductor".into();
                self.conductor = Some(w);
                self.focus = Focus::Conductor;
                self.last_msg = Some("conductor up".into());
                self.dirty.store(true, Ordering::Relaxed);
            }
            Err(e) => {
                // `claude` likely not on PATH — show a clear hint.
                self.last_msg = Some(format!("conductor failed: {e}"));
            }
        }
    }

    fn push_toast(&mut self, source: String, title: String, body: String) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // macOS voice hook — spoken in a background thread so notify isn't
        // blocked on the say binary. Silently no-ops on non-macOS.
        let say_body = body.clone();
        thread::spawn(move || {
            let _ = ProcessCommand::new("say").arg(&say_body).status();
        });
        self.toasts.push_back(Toast { source, title, body, ts });
        while self.toasts.len() > 50 {
            self.toasts.pop_front();
        }
        self.last_msg = Some("toast queued".into());
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn find_window_index(&self, id: u64) -> Option<usize> {
        self.windows.iter().position(|w| w.id == id)
    }

    /// Handle one IPC request, return the response to ship back to the
    /// client. Called from the TUI main thread so all App mutation is
    /// single-threaded.
    fn dispatch_request(&mut self, req: Request) -> Response {
        match req {
            Request::Ls => {
                let panes = self
                    .windows
                    .iter()
                    .map(|w| PaneInfo {
                        id: w.id,
                        title: w.title.clone(),
                        alive: w.alive,
                        rows: w.rows,
                        cols: w.cols,
                        agent: w.agent.clone(),
                        state: w.state,
                    })
                    .collect();
                Response::Ls { panes }
            }
            Request::AgentState { id, state } => match self.find_window_index(id) {
                Some(i) => {
                    self.windows[i].state = state;
                    self.dirty.store(true, Ordering::Relaxed);
                    Response::Ok
                }
                None => Response::Error { message: format!("no pane with id {id}") },
            },
            Request::AgentList => {
                let agents = self
                    .windows
                    .iter()
                    .map(|w| AgentInfo {
                        id: w.id,
                        title: w.title.clone(),
                        agent: w.agent.clone(),
                        state: w.state,
                    })
                    .collect();
                Response::Agents { agents }
            }
            Request::Spawn { cmd } => {
                let (cols, rows) = self.last_body;
                match self.spawn_window(rows, cols, cmd.as_deref()) {
                    Ok(id) => Response::Spawned {
                        id,
                        title: format!("win-{id}"),
                    },
                    Err(e) => Response::Error { message: e.to_string() },
                }
            }
            Request::SendKeys { id, text } => match self.find_window_index(id) {
                Some(i) => {
                    self.windows[i].write(text.as_bytes());
                    Response::Ok
                }
                None => Response::Error { message: format!("no pane with id {id}") },
            },
            Request::CapturePane { id } => match self.find_window_index(id) {
                Some(i) => Response::Captured {
                    text: capture_pane_text(&self.windows[i]),
                },
                None => Response::Error { message: format!("no pane with id {id}") },
            },
            Request::KillPane { id } => match self.find_window_index(id) {
                Some(i) => {
                    self.windows[i].kill();
                    self.windows.remove(i);
                    if self.windows.is_empty() {
                        self.active = None;
                        self.show_help = true;
                        self.view = View::Single;
                    } else if matches!(self.active, Some(a) if a >= self.windows.len()) {
                        self.active = Some(self.windows.len() - 1);
                    }
                    self.dirty.store(true, Ordering::Relaxed);
                    Response::Ok
                }
                None => Response::Error { message: format!("no pane with id {id}") },
            },
            Request::Notify { source, title, body } => {
                self.push_toast(source, title, body);
                Response::Ok
            }
            Request::KillServer => {
                self.quit = true;
                Response::Ok
            }
        }
    }

    /// Internal spawn used by both the keybind path and the IPC server.
    /// `cmd_override` lets the IPC `spawn` request swap the default shell
    /// for `claude` etc.
    fn spawn_window(
        &mut self,
        rows: u16,
        cols: u16,
        cmd_override: Option<&str>,
    ) -> Result<u64> {
        let mut cmd = match cmd_override {
            Some(c) => {
                // Run through the user's shell so `peemux spawn "claude --resume"`
                // tokenizes the same way it would on the command line.
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
                let mut b = CommandBuilder::new(shell);
                b.args(["-c", c]);
                b
            }
            None => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
                CommandBuilder::new(shell)
            }
        };
        cmd.env("TERM", "xterm-256color");
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut w = Window::new(id, cmd, rows, cols, self.dirty.clone())?;
        // Auto-tag the agent name from the first token of an explicit spawn
        // command — e.g. `peemux spawn claude` → agent=Some("claude"). Plain
        // shell windows (from Ctrl-b c) stay agent=None.
        if let Some(c) = cmd_override {
            if let Some(tok) = c.split_whitespace().next() {
                let last = std::path::Path::new(tok)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(tok)
                    .to_string();
                if matches!(last.as_str(), "claude" | "codex" | "aider") {
                    w.agent = Some(last.clone());
                    w.title = last;
                }
            }
        }
        self.windows.push(w);
        self.active = Some(self.windows.len() - 1);
        self.show_help = false;
        self.last_msg = Some(format!("spawned win-{id}"));
        self.dirty.store(true, Ordering::Relaxed);
        Ok(id)
    }

    fn toggle_view(&mut self) {
        if self.windows.is_empty() {
            self.last_msg = Some("no windows — Ctrl-b c to create".into());
            return;
        }
        self.view = match self.view {
            View::Single => View::Wall,
            View::Wall => View::Single,
        };
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Move `active` spatially in the wall's grid. Works in either view —
    /// in single view the underlying active changes (you'll see it the next
    /// time you hit Ctrl-b w). Stays put at edges (no wrap).
    fn move_active_spatial(&mut self, dx: isize, dy: isize) {
        if self.windows.is_empty() {
            return;
        }
        let n = self.windows.len();
        let cols = (n as f32).sqrt().ceil() as usize;
        let max_r = n.div_ceil(cols);
        let cur = self.active.unwrap_or(0);
        let r = cur / cols;
        let c = cur % cols;

        let new_c = if dx < 0 {
            c.saturating_sub((-dx) as usize)
        } else {
            (c + dx as usize).min(cols - 1)
        };
        let new_r = if dy < 0 {
            r.saturating_sub((-dy) as usize)
        } else {
            (r + dy as usize).min(max_r.saturating_sub(1))
        };
        let candidate = (new_r * cols + new_c).min(n - 1);
        self.active = Some(candidate);
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn new_window(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.spawn_window(rows, cols, None).map(|_| ())
    }

    fn kill_active(&mut self) {
        if let Some(i) = self.active {
            self.windows[i].kill();
            self.windows.remove(i);
            if self.windows.is_empty() {
                self.active = None;
                self.show_help = true;
                self.view = View::Single;
            } else if i >= self.windows.len() {
                self.active = Some(self.windows.len() - 1);
            }
            self.last_msg = Some("window killed".into());
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Remove windows whose child has exited (e.g. user typed `exit` or hit
    /// Ctrl-D). Keeps `active` pointed at a sensible window when possible.
    fn reap_dead(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        let before = self.windows.len();
        let active_id: Option<u64> = self
            .active
            .and_then(|i| self.windows.get(i))
            .map(|w| w.id);
        self.windows.retain(|w| w.alive);
        if self.windows.len() == before {
            return;
        }
        if self.windows.is_empty() {
            self.active = None;
            self.show_help = true;
            self.view = View::Single;
        } else if let Some(prev_id) = active_id {
            self.active = self
                .windows
                .iter()
                .position(|w| w.id == prev_id)
                .or(Some(self.windows.len() - 1));
        } else {
            self.active = Some(0);
        }
        self.last_msg = Some("window exited".into());
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn cycle(&mut self, delta: isize) {
        if self.windows.is_empty() {
            return;
        }
        let n = self.windows.len() as isize;
        let cur = self.active.unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(n) as usize;
        self.active = Some(next);
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn jump(&mut self, idx: usize) {
        if idx < self.windows.len() {
            self.active = Some(idx);
            self.dirty.store(true, Ordering::Relaxed);
        }
    }
}

// ─── alacritty_terminal → ratatui ───────────────────────────────────────────
/// Resolve an alacritty AnsiColor to a ratatui Color. Named colors are mapped
/// to the matching 8/16-color indexed entry so the host terminal's palette
/// shows through (transparency-safe — same reason we don't paint a base bg).
fn ansi_to_ratatui(c: AnsiColor) -> Color {
    match c {
        AnsiColor::Spec(Rgb { r, g, b }) => Color::Rgb(r, g, b),
        AnsiColor::Indexed(i) => Color::Indexed(i),
        AnsiColor::Named(n) => named_to_ratatui(n),
    }
}

fn named_to_ratatui(n: NamedColor) -> Color {
    use NamedColor::*;
    match n {
        Black | DimBlack => Color::Indexed(0),
        Red | DimRed => Color::Indexed(1),
        Green | DimGreen => Color::Indexed(2),
        Yellow | DimYellow => Color::Indexed(3),
        Blue | DimBlue => Color::Indexed(4),
        Magenta | DimMagenta => Color::Indexed(5),
        Cyan | DimCyan => Color::Indexed(6),
        White | DimWhite => Color::Indexed(7),
        BrightBlack => Color::Indexed(8),
        BrightRed => Color::Indexed(9),
        BrightGreen => Color::Indexed(10),
        BrightYellow => Color::Indexed(11),
        BrightBlue => Color::Indexed(12),
        BrightMagenta => Color::Indexed(13),
        BrightCyan => Color::Indexed(14),
        BrightWhite | BrightForeground => Color::Indexed(15),
        Foreground | DimForeground => Color::Reset,
        Background => Color::Reset,
        Cursor => Color::Reset,
    }
}

struct PtyWidget<'a> {
    term: &'a Term<VoidListener>,
}

impl<'a> Widget for PtyWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let content = self.term.renderable_content();
        let total_lines = self.term.screen_lines() as i32;
        let display_offset = content.display_offset as i32;
        for cell in content.display_iter {
            // Wide-char spacer is rendered as part of the previous cell's
            // glyph — skip to avoid double-painting.
            if cell.cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                continue;
            }
            // alacritty grid line indices: scrollback rows are negative,
            // visible rows go 0..screen_lines. Convert to widget coords by
            // adding display_offset and clamping. Drop anything outside the
            // visible area (covers scrollback that overflows the viewport).
            let line_idx = cell.point.line.0 + display_offset;
            if line_idx < 0 || line_idx >= total_lines {
                continue;
            }
            let col = cell.point.column.0;
            if col >= area.width as usize {
                continue;
            }
            let row = line_idx as u16;
            if row >= area.height {
                continue;
            }

            let target = &mut buf[(area.x + col as u16, area.y + row)];
            let mut tmp = [0u8; 4];
            let s = cell.cell.c.encode_utf8(&mut tmp);
            target.set_symbol(if cell.cell.c == '\0' || cell.cell.c == ' ' {
                " "
            } else {
                // SAFETY: tmp's lifetime — we copy via set_symbol which
                // interns the string into the buffer cell.
                unsafe { std::str::from_utf8_unchecked(s.as_bytes()) }
            });

            let mut style = Style::default()
                .fg(ansi_to_ratatui(cell.cell.fg))
                .bg(ansi_to_ratatui(cell.cell.bg));

            let mut m = Modifier::empty();
            let flags = cell.cell.flags;
            if flags.contains(CellFlags::BOLD) {
                m |= Modifier::BOLD;
            }
            if flags.contains(CellFlags::ITALIC) {
                m |= Modifier::ITALIC;
            }
            if flags.contains(CellFlags::UNDERLINE) {
                m |= Modifier::UNDERLINED;
            }
            if flags.contains(CellFlags::INVERSE) {
                m |= Modifier::REVERSED;
            }
            if flags.contains(CellFlags::DIM) {
                m |= Modifier::DIM;
            }
            if flags.contains(CellFlags::STRIKEOUT) {
                m |= Modifier::CROSSED_OUT;
            }
            if flags.contains(CellFlags::HIDDEN) {
                m |= Modifier::HIDDEN;
            }
            if !m.is_empty() {
                style = style.add_modifier(m);
            }
            target.set_style(style);
        }
    }
}

/// Dump a pane's visible screen to plain text (no ANSI). One row per line,
/// trailing whitespace stripped, trailing blank rows dropped. Used by the
/// `peemux capture-pane <id>` IPC op so the conductor can "see" what a
/// worker said.
fn capture_pane_text(w: &Window) -> String {
    let Ok(state) = w.vt.lock() else {
        return String::new();
    };
    let content = state.term.renderable_content();
    let rows = state.term.screen_lines();
    let cols = state.term.columns();
    let display_offset = content.display_offset as i32;
    let mut grid: Vec<Vec<char>> = vec![vec![' '; cols]; rows];
    for cell in content.display_iter {
        if cell.cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
            continue;
        }
        let row = cell.point.line.0 + display_offset;
        if row < 0 || row >= rows as i32 {
            continue;
        }
        let col = cell.point.column.0;
        if col >= cols {
            continue;
        }
        let ch = cell.cell.c;
        grid[row as usize][col] = if ch == '\0' { ' ' } else { ch };
    }
    let mut lines: Vec<String> = grid
        .into_iter()
        .map(|row| {
            let s: String = row.into_iter().collect();
            s.trim_end().to_string()
        })
        .collect();
    while lines.last().map(|s| s.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Visible cursor position in widget coords, or None if hidden / off-screen.
fn cursor_xy(term: &Term<VoidListener>, area: Rect) -> Option<(u16, u16)> {
    let content = term.renderable_content();
    if content.cursor.shape == CursorShape::Hidden {
        return None;
    }
    let Point { line, column } = content.cursor.point;
    let row = line.0 + content.display_offset as i32;
    if row < 0 || row >= area.height as i32 {
        return None;
    }
    let col = column.0;
    if col >= area.width as usize {
        return None;
    }
    Some((area.x + col as u16, area.y + row as u16))
}

// ─── key → PTY bytes ────────────────────────────────────────────────────────
fn key_to_bytes(k: &KeyEvent) -> Option<Vec<u8>> {
    use KeyCode::*;
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    match k.code {
        Char(c) => {
            if ctrl && c.is_ascii() {
                // Ctrl-letter → control byte
                let cl = c.to_ascii_lowercase() as u8;
                if cl.is_ascii_alphabetic() {
                    return Some(vec![cl - b'a' + 1]);
                }
                // Common Ctrl-symbol mappings
                let b = match cl {
                    b' ' => Some(0),
                    b'@' => Some(0),
                    b'[' => Some(0x1b),
                    b'\\' => Some(0x1c),
                    b']' => Some(0x1d),
                    b'^' => Some(0x1e),
                    b'_' => Some(0x1f),
                    _ => None,
                };
                if let Some(byte) = b {
                    return Some(vec![byte]);
                }
            }
            let mut s = String::new();
            if alt {
                s.push('\x1b');
            }
            s.push(c);
            Some(s.into_bytes())
        }
        Enter => Some(vec![b'\r']),
        Tab => Some(vec![b'\t']),
        BackTab => Some(b"\x1b[Z".to_vec()),
        Backspace => Some(vec![0x7f]),
        Esc => Some(vec![0x1b]),
        Up => Some(b"\x1b[A".to_vec()),
        Down => Some(b"\x1b[B".to_vec()),
        Right => Some(b"\x1b[C".to_vec()),
        Left => Some(b"\x1b[D".to_vec()),
        Home => Some(b"\x1b[H".to_vec()),
        End => Some(b"\x1b[F".to_vec()),
        PageUp => Some(b"\x1b[5~".to_vec()),
        PageDown => Some(b"\x1b[6~".to_vec()),
        Insert => Some(b"\x1b[2~".to_vec()),
        Delete => Some(b"\x1b[3~".to_vec()),
        F(n) => {
            let seq: Vec<u8> = match n {
                1 => b"\x1bOP".to_vec(),
                2 => b"\x1bOQ".to_vec(),
                3 => b"\x1bOR".to_vec(),
                4 => b"\x1bOS".to_vec(),
                5 => b"\x1b[15~".to_vec(),
                6 => b"\x1b[17~".to_vec(),
                7 => b"\x1b[18~".to_vec(),
                8 => b"\x1b[19~".to_vec(),
                9 => b"\x1b[20~".to_vec(),
                10 => b"\x1b[21~".to_vec(),
                11 => b"\x1b[23~".to_vec(),
                12 => b"\x1b[24~".to_vec(),
                _ => return None,
            };
            Some(seq)
        }
        _ => None,
    }
}

// ─── run_tui ────────────────────────────────────────────────────────────────
fn run_tui() -> Result<()> {
    // Fail fast if there's already a peemux server on this socket — two
    // attached TUIs would corrupt each other.
    let sock = socket_path();
    if UnixStream::connect(&sock).is_ok() {
        return Err(anyhow!(
            "another peemux is already running on {}",
            sock.display()
        ));
    }

    let (req_tx, req_rx) = mpsc::channel::<ServerRequest>();
    let _cleanup = spawn_server(req_tx)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = main_loop(&mut terminal, req_rx);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    res
}

fn main_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    req_rx: mpsc::Receiver<ServerRequest>,
) -> Result<()> {
    let mut app = App::new();

    loop {
        // Each tick: size PTYs to their actual visible area (so wall tiles
        // scroll-follow correctly), then poll children and reap any that
        // exited so `exit` / Ctrl-D in the shell closes the window.
        let size = terminal.size()?;
        let body = body_rect(size);
        sync_sizes(&mut app, body);
        // Track the workers' main area so `peemux spawn` from the IPC path
        // sizes new PTYs the same way the keyboard `Ctrl-b c` path does.
        let (_, main) = split_body(body, &app);
        app.last_body = (main.width, main.height);
        // Lazy-spawn the conductor the first frame after the sidebar opens.
        if app.sidebar_visible && app.conductor.is_none() {
            if let Some(sb) = split_body(body, &app).0 {
                let inner = Block::default().borders(Borders::ALL).inner(sidebar_rects(sb).conductor);
                app.ensure_conductor(inner.height.max(1), inner.width.max(1));
            }
        }
        for w in app.windows.iter_mut() {
            w.poll_alive();
        }
        if let Some(c) = app.conductor.as_mut() {
            c.poll_alive();
        }
        app.reap_dead();

        // Run the agent-state heuristic roughly once per second (60 ticks at
        // the 16 ms poll cadence). Cheap string-contains checks, not regex.
        app.tick = app.tick.wrapping_add(1);
        if app.tick % 60 == 0 {
            app.run_heuristics();
        }
        app.ack_active();

        // Drain any IPC requests the server has queued for us. All App
        // mutation lives in this thread — keeps the model simple.
        while let Ok(req) = req_rx.try_recv() {
            let resp = app.dispatch_request(req.req);
            let _ = req.reply.send(resp);
        }

        if app.dirty.swap(false, Ordering::Relaxed) {
            terminal.draw(|f| draw(f, &app))?;
        }

        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    handle_key(&mut app, k);
                }
                Event::Resize(_, _) => {
                    app.dirty.store(true, Ordering::Relaxed);
                }
                Event::Mouse(m) => match m.kind {
                    crossterm::event::MouseEventKind::Down(_) => {
                        handle_mouse_down(&mut app, m.column, m.row, size);
                    }
                    crossterm::event::MouseEventKind::ScrollUp => {
                        handle_mouse_scroll(&mut app, m.column, m.row, size, true);
                    }
                    crossterm::event::MouseEventKind::ScrollDown => {
                        handle_mouse_scroll(&mut app, m.column, m.row, size, false);
                    }
                    _ => {}
                },
                _ => {}
            }
            app.dirty.store(true, Ordering::Relaxed);
        }

        if app.quit {
            // Kill any surviving children so we don't leak processes.
            for w in app.windows.iter_mut() {
                w.kill();
            }
            if let Some(c) = app.conductor.as_mut() {
                c.kill();
            }
            return Ok(());
        }
    }
}

/// Split the body into (optional sidebar, main worker area). Sidebar is
/// hidden automatically when the terminal is too narrow to give workers
/// breathing room.
fn split_body(body: Rect, app: &App) -> (Option<Rect>, Rect) {
    if !app.sidebar_visible {
        return (None, body);
    }
    let sb_w = app.sidebar_width;
    // Require at least ~36 cols for workers and a healthy sidebar minimum.
    if body.width < sb_w + 36 || sb_w < 28 {
        return (None, body);
    }
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(sb_w), Constraint::Min(0)])
        .split(body);
    (Some(chunks[0]), chunks[1])
}

/// Inner rects inside the sidebar. Header (badge), friends, notifications,
/// conductor — top to bottom. Caller is responsible for handling tiny
/// sidebars (each rect may have zero height when squeezed).
struct SidebarRects {
    header: Rect,
    friends: Rect,
    notifs: Rect,
    conductor: Rect,
}

fn sidebar_rects(sidebar: Rect) -> SidebarRects {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Length(5), // friends
            Constraint::Length(5), // notifications
            Constraint::Min(6),    // conductor (greedy)
        ])
        .split(sidebar);
    SidebarRects {
        header: chunks[0],
        friends: chunks[1],
        notifs: chunks[2],
        conductor: chunks[3],
    }
}

/// Size each window's PTY to its *actual visible area* — full main area in
/// single view, tile's inner rect in wall view. Without this, wall-view tiles
/// show the top slice of a body-sized screen and never scroll-follow. Also
/// sizes the conductor PTY (if any) to its inner rect inside the sidebar.
fn sync_sizes(app: &mut App, body: Rect) {
    let (sidebar, main) = split_body(body, app);
    if !app.windows.is_empty() {
        match app.view {
            View::Single => {
                let rows = main.height.max(1);
                let cols = main.width.max(1);
                for w in app.windows.iter_mut() {
                    w.resize(rows, cols);
                }
            }
            View::Wall => {
                let rects = wall_tile_rects(main, app.windows.len());
                for (w, r) in app.windows.iter_mut().zip(rects.iter()) {
                    // -2 for the surrounding Borders::ALL block.
                    let rows = r.height.saturating_sub(2).max(1);
                    let cols = r.width.saturating_sub(2).max(1);
                    w.resize(rows, cols);
                }
            }
        }
    }
    if let (Some(sb), Some(cond)) = (sidebar, app.conductor.as_mut()) {
        let inner = Block::default().borders(Borders::ALL).inner(sidebar_rects(sb).conductor);
        cond.resize(inner.height.max(1), inner.width.max(1));
    }
}

/// Mouse wheel → real scrollback on the pane under the cursor (3 lines per
/// tick). Uses alacritty_terminal's scroll_display so any tail of new PTY
/// output snaps us back to the bottom — same as a real terminal. Doesn't
/// change the active pane. Also scrolls the conductor PTY when the cursor is
/// over the sidebar's conductor area.
fn handle_mouse_scroll(
    app: &mut App,
    col: u16,
    row: u16,
    size: ratatui::layout::Size,
    up: bool,
) {
    let body = body_rect(size);
    if !rect_contains(body, col, row) {
        return;
    }
    let (sidebar, main) = split_body(body, app);
    let delta = if up { 3 } else { -3 };

    // Sidebar first — conductor PTY scroll.
    if let Some(sb) = sidebar {
        let cond_inner = Block::default().borders(Borders::ALL).inner(sidebar_rects(sb).conductor);
        if rect_contains(cond_inner, col, row) {
            if let Some(w) = app.conductor.as_mut() {
                w.scroll(delta);
                app.dirty.store(true, Ordering::Relaxed);
            }
            return;
        }
    }

    if app.windows.is_empty() || !rect_contains(main, col, row) {
        return;
    }
    let target = match app.view {
        View::Wall => {
            let rects = wall_tile_rects(main, app.windows.len());
            rects.iter().enumerate().find_map(|(i, r)| {
                if rect_contains(*r, col, row) { Some(i) } else { None }
            })
        }
        View::Single => app.active,
    };
    if let Some(i) = target {
        if let Some(w) = app.windows.get_mut(i) {
            w.scroll(delta);
            app.dirty.store(true, Ordering::Relaxed);
        }
    }
}

/// Body area is full terminal minus top bar (1) and status bar (1).
fn pty_body_size((w, h): (u16, u16)) -> (u16, u16) {
    let rows = h.saturating_sub(2).max(1);
    let cols = w.max(1);
    (rows, cols)
}

/// Body rectangle (in screen coords) — same area the PTYs render into.
fn body_rect(size: ratatui::layout::Size) -> Rect {
    Rect {
        x: 0,
        y: 1,
        width: size.width,
        height: size.height.saturating_sub(2),
    }
}

/// Compute the rect for each tile in the wall grid, matching draw_wall's
/// layout. Used by the mouse hit-tester.
fn wall_tile_rects(body: Rect, n: usize) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    let cols = (n as f32).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    let row_constraints: Vec<Constraint> =
        (0..rows).map(|_| Constraint::Ratio(1, rows as u32)).collect();
    let col_constraints: Vec<Constraint> =
        (0..cols).map(|_| Constraint::Ratio(1, cols as u32)).collect();
    let row_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(body);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let r = i / cols;
        let c = i % cols;
        if r >= row_areas.len() {
            break;
        }
        let cell_row = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(col_constraints.clone())
            .split(row_areas[r]);
        if c < cell_row.len() {
            out.push(cell_row[c]);
        }
    }
    out
}

fn rect_contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

fn handle_mouse_down(app: &mut App, col: u16, row: u16, size: ratatui::layout::Size) {
    let body = body_rect(size);
    if !rect_contains(body, col, row) {
        return;
    }
    let (sidebar, main) = split_body(body, app);

    // Click inside the conductor area focuses the conductor.
    if let Some(sb) = sidebar {
        let cond_inner = Block::default().borders(Borders::ALL).inner(sidebar_rects(sb).conductor);
        if rect_contains(cond_inner, col, row) {
            if app.conductor.is_some() {
                app.focus = Focus::Conductor;
                app.last_msg = Some("focus → conductor".into());
                app.dirty.store(true, Ordering::Relaxed);
            }
            return;
        }
    }

    // Click in the worker area: focus shifts to workers, and in wall view
    // picks the tile.
    if rect_contains(main, col, row) {
        if app.focus != Focus::Workers {
            app.focus = Focus::Workers;
            app.dirty.store(true, Ordering::Relaxed);
        }
        if app.view == View::Wall && !app.windows.is_empty() {
            let rects = wall_tile_rects(main, app.windows.len());
            for (i, r) in rects.iter().enumerate() {
                if rect_contains(*r, col, row) {
                    app.active = Some(i);
                    app.last_msg = Some(format!("active → {}", i + 1));
                    app.dirty.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    }
}

// ─── input ──────────────────────────────────────────────────────────────────
fn handle_key(app: &mut App, k: KeyEvent) {
    // Prefix dispatch wins over everything else.
    if app.prefix_armed {
        app.prefix_armed = false;
        match k.code {
            KeyCode::Char('c') => {
                let (rows, cols) = pty_body_size(crossterm::terminal::size().unwrap_or((80, 24)));
                if let Err(e) = app.new_window(rows, cols) {
                    app.last_msg = Some(format!("spawn failed: {e}"));
                }
            }
            KeyCode::Char('n') => app.cycle(1),
            KeyCode::Char('p') => app.cycle(-1),
            KeyCode::Char('w') => app.toggle_view(),
            KeyCode::Tab => app.toggle_sidebar(),
            KeyCode::Char('o') => app.toggle_focus(),
            KeyCode::Up => app.move_active_spatial(0, -1),
            KeyCode::Down => app.move_active_spatial(0, 1),
            KeyCode::Left => app.move_active_spatial(-1, 0),
            KeyCode::Right => app.move_active_spatial(1, 0),
            KeyCode::Char('&') | KeyCode::Char('x') => app.kill_active(),
            KeyCode::Char('?') => {
                app.show_help = !app.show_help;
                app.last_msg = Some(if app.show_help { "help on" } else { "help off" }.into());
            }
            KeyCode::Char('d') => app.quit = true,
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let idx = c.to_digit(10).unwrap() as usize;
                let idx = if idx == 0 { 9 } else { idx - 1 };
                app.jump(idx);
            }
            _ => {
                app.last_msg = Some(format!("unknown prefix: {:?}", k.code));
            }
        }
        return;
    }

    // Arm prefix on Ctrl-B.
    if let KeyCode::Char(c) = k.code {
        if k.modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'b') {
            app.prefix_armed = true;
            app.last_msg = Some("prefix".into());
            return;
        }
    }

    // Forward to whichever PTY currently owns focus. Conductor wins when
    // the sidebar is visible and the user has focused it; otherwise we fall
    // back to the active worker.
    let Some(bytes) = key_to_bytes(&k) else { return };
    let routed_to_conductor = app.focus == Focus::Conductor
        && app.sidebar_visible
        && app.conductor.is_some();
    if routed_to_conductor {
        if let Some(w) = app.conductor.as_mut() {
            w.write(&bytes);
        }
        return;
    }
    if let Some(i) = app.active {
        if let Some(w) = app.windows.get_mut(i) {
            w.write(&bytes);
        }
    }
}

// ─── render ─────────────────────────────────────────────────────────────────
fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top bar (logo badge + tab strip + prefix indicator)
            Constraint::Min(0),    // body (welcome OR active PTY)
            Constraint::Length(1), // bottom hints
        ])
        .split(area);

    draw_top_bar(f, chunks[0], app);
    draw_body(f, chunks[1], app);
    draw_bottom_bar(f, chunks[2], app);
}

fn draw_top_bar(f: &mut Frame, area: Rect, app: &App) {
    // Layout: [PEEMUX badge]  tab strip ...   [view chip]  [prefix indicator]
    let prefix_chip = if app.prefix_armed { " ⌨ PREFIX " } else { "" };
    let view_chip = match app.view {
        View::Single => " SINGLE ",
        View::Wall => "  WALL  ",
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(10), // " PEEMUX "
            Constraint::Min(0),     // tabs
            Constraint::Length(view_chip.chars().count() as u16),
            Constraint::Length(prefix_chip.chars().count() as u16),
        ])
        .split(area);

    let badge = Line::from(Span::styled(
        " PEEMUX ",
        Style::default().bg(C_PINK).fg(Color::Black).bold(),
    ));
    f.render_widget(Paragraph::new(badge), cols[0]);

    // Tab strip — windows by index. Active in pink, others dim.
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    if app.windows.is_empty() {
        spans.push(Span::styled(
            "no windows  ",
            Style::default().fg(C_DIM).italic(),
        ));
        spans.push(Span::styled(
            "(Ctrl-b c to create)",
            Style::default().fg(C_DIM),
        ));
    } else {
        for (i, w) in app.windows.iter().enumerate() {
            let active = app.active == Some(i);
            let alive_mark = if w.alive { "" } else { "✗" };
            let style = if active {
                Style::default().bg(C_PINK).fg(Color::Black).bold()
            } else {
                Style::default().bg(C_PANEL).fg(C_FG)
            };
            // Per-tab agent dot prefix when the pane has an attached agent.
            if w.state != AgentState::None {
                spans.push(Span::styled(
                    format!(" {} ", w.state.dot()),
                    Style::default().fg(w.state.color()).bg(if active { C_PINK } else { C_PANEL }),
                ));
            }
            let label = format!(" {}:{}{} ", i + 1, w.title, alive_mark);
            spans.push(Span::styled(label, style));
            spans.push(Span::raw(" "));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), cols[1]);

    let view_style = match app.view {
        View::Single => Style::default().bg(C_PANEL).fg(C_FG).bold(),
        View::Wall => Style::default().bg(C_PINK).fg(Color::Black).bold(),
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(view_chip, view_style))),
        cols[2],
    );

    if !prefix_chip.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                prefix_chip,
                Style::default().bg(C_ORANGE).fg(Color::Black).bold(),
            ))),
            cols[3],
        );
    }
}

fn draw_body(f: &mut Frame, area: Rect, app: &App) {
    let (sidebar, main) = split_body(area, app);
    if let Some(sb) = sidebar {
        draw_sidebar(f, sb, app);
    }
    if app.windows.is_empty() {
        draw_welcome(f, main);
        return;
    }
    match app.view {
        View::Single => {
            if let Some(w) = app.active.and_then(|i| app.windows.get(i)) {
                draw_pty(f, main, w);
                if app.focus == Focus::Workers {
                    if let Ok(state) = w.vt.lock() {
                        if let Some((x, y)) = cursor_xy(&state.term, main) {
                            f.set_cursor_position((x, y));
                        }
                    }
                }
            }
            if app.show_help {
                draw_help_overlay(f, main);
            }
        }
        View::Wall => draw_wall(f, main, app),
    }
}

/// Sidebar: PEEMUX badge header, friends placeholder, recent notifications,
/// pinned conductor PTY. Whole sidebar gets a right-edge divider.
fn draw_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let rects = sidebar_rects(area);

    // Header: badge + tagline.
    let header_lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                " PEEMUX ",
                Style::default().bg(C_PINK).fg(Color::Black).bold(),
            ),
            Span::raw("  "),
            Span::styled("agent cockpit", Style::default().fg(C_DIM).italic()),
        ]),
        Line::from(""),
    ];
    f.render_widget(Paragraph::new(header_lines), rects.header);

    // Friends list (M3+ stub — wire to peers in M4).
    draw_sidebar_section(
        f,
        rects.friends,
        "FRIENDS",
        vec![
            Line::from(Span::styled(
                "  ✦ no peers yet",
                Style::default().fg(C_DIM).italic(),
            )),
            Line::from(Span::styled(
                "  (M3+)",
                Style::default().fg(C_DIM).dim(),
            )),
        ],
    );

    // Notifications (last 3 toasts, newest first).
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let notif_lines: Vec<Line> = if app.toasts.is_empty() {
        vec![Line::from(Span::styled(
            "  · no notifications",
            Style::default().fg(C_DIM).italic(),
        ))]
    } else {
        app.toasts
            .iter()
            .rev()
            .take(3)
            .map(|t| {
                let age = now.saturating_sub(t.ts);
                let age_label = if age < 60 {
                    format!("{}s", age)
                } else if age < 3600 {
                    format!("{}m", age / 60)
                } else {
                    format!("{}h", age / 3600)
                };
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled("●", Style::default().fg(C_ORANGE)),
                    Span::raw(" "),
                    Span::styled(t.source.clone(), Style::default().fg(C_PINK).bold()),
                    Span::styled(": ", Style::default().fg(C_DIM)),
                    Span::styled(t.title.clone(), Style::default().fg(C_FG)),
                    Span::raw(" "),
                    Span::styled(format!("({age_label})"), Style::default().fg(C_DIM)),
                ])
            })
            .collect()
    };
    draw_sidebar_section(f, rects.notifs, "NOTIFICATIONS", notif_lines);

    // Conductor pane — bordered, pink when focused.
    let cond_focused = app.focus == Focus::Conductor;
    let title = Line::from(vec![
        Span::styled(" ◆ ", Style::default().fg(C_PINK).bold()),
        Span::styled(
            "CONDUCTOR",
            if cond_focused {
                Style::default().fg(C_PINK).bold()
            } else {
                Style::default().fg(C_GREEN).bold()
            },
        ),
        Span::raw(" "),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if cond_focused { C_PINK } else { C_PANEL }))
        .title(title);
    let inner = block.inner(rects.conductor);
    f.render_widget(block, rects.conductor);

    match &app.conductor {
        Some(w) if w.alive => {
            if let Ok(state) = w.vt.lock() {
                f.render_widget(PtyWidget { term: &state.term }, inner);
                if cond_focused {
                    if let Some((x, y)) = cursor_xy(&state.term, inner) {
                        f.set_cursor_position((x, y));
                    }
                }
            }
        }
        Some(_) => {
            let lines = vec![
                Line::from(""),
                Line::from(centered_span(
                    inner.width,
                    "conductor exited",
                    Style::default().fg(C_ORANGE).bold(),
                )),
                Line::from(""),
                Line::from(centered_span(
                    inner.width,
                    "Ctrl-b Tab to hide,",
                    Style::default().fg(C_DIM),
                )),
                Line::from(centered_span(
                    inner.width,
                    "then again to respawn",
                    Style::default().fg(C_DIM),
                )),
            ];
            f.render_widget(Paragraph::new(lines), inner);
        }
        None => {
            let lines = vec![
                Line::from(""),
                Line::from(centered_span(
                    inner.width,
                    "spawning conductor…",
                    Style::default().fg(C_DIM).italic(),
                )),
            ];
            f.render_widget(Paragraph::new(lines), inner);
        }
    }
}

fn draw_sidebar_section(f: &mut Frame, area: Rect, label: &str, body: Vec<Line>) {
    if area.height == 0 {
        return;
    }
    let mut lines: Vec<Line> = Vec::with_capacity(body.len() + 1);
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(label, Style::default().fg(C_GREEN).bold()),
        Span::raw(" "),
        Span::styled(
            "─".repeat(area.width.saturating_sub(label.len() as u16 + 3) as usize),
            Style::default().fg(C_PANEL),
        ),
    ]));
    for l in body {
        lines.push(l);
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn centered_span(area_w: u16, text: &str, style: Style) -> Span<'static> {
    let text_w = text.chars().count() as u16;
    let pad = area_w.saturating_sub(text_w) / 2;
    Span::styled(
        format!("{}{}", " ".repeat(pad as usize), text),
        style,
    )
}

fn draw_wall(f: &mut Frame, area: Rect, app: &App) {
    let n = app.windows.len();
    if n == 0 {
        draw_welcome(f, area);
        return;
    }
    let rects = wall_tile_rects(area, n);

    for (i, (w, cell)) in app.windows.iter().zip(rects.iter()).enumerate() {
        let cell = *cell;
        let is_active = app.active == Some(i);
        let border_color = if is_active { C_PINK } else { C_PANEL };

        let alive_mark = if w.alive { "" } else { " ✗" };
        let title_style_idx = if is_active {
            Style::default().bg(C_PINK).fg(Color::Black).bold()
        } else {
            Style::default().fg(C_ORANGE).bold()
        };
        let title_style_name = if is_active {
            Style::default().fg(C_PINK).bold()
        } else {
            Style::default().fg(C_FG).bold()
        };
        let mut title_spans: Vec<Span> = vec![
            Span::raw(" "),
            Span::styled(format!(" {} ", i + 1), title_style_idx),
            Span::raw(" "),
        ];
        if w.state != AgentState::None {
            title_spans.push(Span::styled(
                format!("{} ", w.state.dot()),
                Style::default().fg(w.state.color()).bold(),
            ));
        }
        title_spans.push(Span::styled(w.title.clone(), title_style_name));
        title_spans.push(Span::styled(alive_mark, Style::default().fg(Color::Red)));
        title_spans.push(Span::raw(" "));
        let title = Line::from(title_spans);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(title);
        let inner = block.inner(cell);
        f.render_widget(block, cell);

        if let Ok(state) = w.vt.lock() {
            f.render_widget(PtyWidget { term: &state.term }, inner);
        }
    }

    // In wall view, draw the cursor inside the active tile — but only when
    // keyboard focus is on the workers. With the conductor focused, the
    // sidebar render owns the cursor instead.
    if app.focus == Focus::Workers {
        if let Some(i) = app.active {
            if let (Some(w), Some(cell)) = (app.windows.get(i), rects.get(i)) {
                let inner = Block::default().borders(Borders::ALL).inner(*cell);
                if let Ok(state) = w.vt.lock() {
                    if let Some((x, y)) = cursor_xy(&state.term, inner) {
                        f.set_cursor_position((x, y));
                    }
                }
            }
        }
    }
}

/// Render a PTY to `area`. Cursor placement is the caller's responsibility —
/// at most one PTY on screen can own the terminal cursor at a time, so the
/// focus-aware caller (draw_body / draw_wall / draw_sidebar) decides.
fn draw_pty(f: &mut Frame, area: Rect, w: &Window) {
    if let Ok(state) = w.vt.lock() {
        f.render_widget(PtyWidget { term: &state.term }, area);
    }
}

fn draw_welcome(f: &mut Frame, area: Rect) {
    // Center the logo + keybinds vertically + horizontally.
    let logo_h = LOGO.len() as u16;
    let kb_h = (KEYBINDS.len() + 4) as u16; // +4 for header, blank, footer hint, gap
    let total = logo_h + 2 + kb_h;
    let top_pad = area.height.saturating_sub(total) / 2;

    let mut lines: Vec<Line> = Vec::new();
    for _ in 0..top_pad {
        lines.push(Line::from(""));
    }
    for (i, row) in LOGO.iter().enumerate() {
        // Subtle pink → orange gradient down the logo for a little flair.
        let color = match i {
            0..=1 => C_PINK,
            2..=3 => Color::Rgb(0xff, 0x55, 0x88),
            _ => C_ORANGE,
        };
        lines.push(centered_line(area.width, row, Style::default().fg(color).bold()));
    }
    lines.push(Line::from(""));
    lines.push(centered_line(
        area.width,
        "the terminal for the agent era",
        Style::default().fg(C_DIM).italic(),
    ));
    lines.push(Line::from(""));
    lines.push(centered_line(
        area.width,
        "keyboard shortcuts",
        Style::default().fg(C_GREEN).bold(),
    ));
    lines.push(Line::from(""));
    for (k, d) in KEYBINDS {
        let label = format!("{:<14}  {}", k, d);
        lines.push(centered_line(area.width, &label, Style::default().fg(C_FG)));
    }
    lines.push(Line::from(""));
    lines.push(centered_line(
        area.width,
        "press  Ctrl-b  then  c  to spawn your first window",
        Style::default().fg(C_ORANGE).bold(),
    ));
    lines.push(centered_line(
        area.width,
        "press  Ctrl-b  then  Tab  to open the conductor sidebar",
        Style::default().fg(C_PINK).bold(),
    ));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn centered_line(area_w: u16, text: &str, style: Style) -> Line<'static> {
    let text_w = text.chars().count() as u16;
    let pad = area_w.saturating_sub(text_w) / 2;
    Line::from(vec![
        Span::raw(" ".repeat(pad as usize)),
        Span::styled(text.to_string(), style),
    ])
}

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    // Modal box bottom-right, opaque so it reads over a busy PTY.
    let width: u16 = 46;
    let height: u16 = (KEYBINDS.len() + 4) as u16;
    if area.width < width + 2 || area.height < height + 2 {
        return;
    }
    let x = area.x + area.width.saturating_sub(width + 2);
    let y = area.y + area.height.saturating_sub(height + 2);
    let rect = Rect { x, y, width, height };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_PINK))
        .title(Line::from(vec![
            Span::styled(" peemux ", Style::default().bg(C_PINK).fg(Color::Black).bold()),
            Span::styled(" keybinds ", Style::default().fg(C_PINK).bold()),
        ]))
        .style(Style::default().bg(C_BG).fg(C_FG));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    for (k, d) in KEYBINDS {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:<14}", k), Style::default().fg(C_ORANGE).bold()),
            Span::styled(*d, Style::default().fg(C_FG)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_bottom_bar(f: &mut Frame, area: Rect, app: &App) {
    let nwin = app.windows.len();
    let active_label = match app.active {
        Some(i) => format!(" {}/{} ", i + 1, nwin),
        None => " 0/0 ".into(),
    };
    let left = Line::from(vec![
        Span::styled(
            " PEEMUX ",
            Style::default().bg(C_PINK).fg(Color::Black).bold(),
        ),
        Span::styled(active_label, Style::default().bg(C_PANEL).fg(C_GREEN)),
        Span::styled(
            "  Ctrl-b  ·  c new  n/p switch  w wall  Tab sidebar  o focus  x kill  ? help  d quit",
            Style::default().fg(C_DIM),
        ),
    ]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(40)])
        .split(area);
    f.render_widget(Paragraph::new(left), cols[0]);

    if let Some(msg) = &app.last_msg {
        let s = format!("● {}", msg);
        let truncated: String = s.chars().take(38).collect();
        let right = Line::from(Span::styled(
            format!(" {:>38} ", truncated),
            Style::default().fg(C_ORANGE),
        ));
        f.render_widget(Paragraph::new(right), cols[1]);
    }
}
