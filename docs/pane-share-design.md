# Pane Sharing — Design

Share a live, read-only view of one local pane with a peemux friend over the
existing Tailscale peer channel. The friend receives a share link, accepts it,
and the pane appears in their pane list as a live remote pane.

## UX

Sharer:

```
peemux share-pane 2 alice        # share pane 2 with peer "alice"
# → prints: shared pane 2 with alice — link: peemux://join?host=100.x.y.z&token=Ab3xK9...
peemux share stop 2              # stop sharing pane 2 (ends all viewers)
peemux share list                # list active outgoing shares + viewers
```

Receiver (alice):

- Toast appears: `tyler wants to share pane "fix-tests" — Ctrl-b y to accept`
- Penguin waves. Friend name lights pink.
- Accept via ANY of:
  1. `Ctrl-b y` — accept the newest pending share offer
  2. mouse click on the offer toast in the NOTIFICATIONS section
  3. `peemux share accept` (CLI/IPC — so a conductor agent can accept too)
- On accept, a new pane appears: title `🔗 fix-tests (tyler)`, live view of the
  sharer's pane. Read-only: send-keys to it returns an error.
- Killing the remote pane (Ctrl-b x or kill-pane) detaches cleanly.

## Link format

`peemux://join?host=<tailscale-ip>&token=<token>`

The token is a random 24-char alphanumeric capability generated per share.
The link is embedded in the ShareOffer wire message AND included in the
human-readable fallback text, so it is visible/copyable even if the receiving
side ever fails to parse the structured offer.

## Wire protocol (src/peers.rs)

Extend the existing newline-delimited-JSON-over-TCP-9867 `Wire` enum. All new
variants are additive; old peers simply fail to parse unknown `type` tags and
drop the line (verify current behavior degrades gracefully — if not, guard).

```rust
enum Wire {
    Hello { user: String },
    Message { from: String, text: String },
    Ack,
    // -- new --
    ShareOffer { from: String, title: String, host: String, token: String },
    JoinShare  { token: String, user: String },          // opens a STREAMING conn
    ShareAccepted { title: String, rows: u16, cols: u16 },
    PaneFrame  { rows: u16, cols: u16, seq: String },    // full ANSI screen snapshot
    ShareEnd   { reason: String },
    ShareError { message: String },
}
```

Connection model:
- ShareOffer: one-shot connection sharer→receiver's listener (like Message/Ack).
- JoinShare: receiver connects to `host:9867`, sends JoinShare, and the
  connection STAYS OPEN as the frame stream: sharer replies ShareAccepted, then
  an initial PaneFrame, then PaneFrames as the pane changes, then ShareEnd.
  The existing `handle_incoming` handles one line then closes — the JoinShare
  arm must instead hand the socket to a long-lived streaming loop.

## Frame encoding — full ANSI snapshots

Simplest correct approach; no delta tracking, no protocol state.

New fn `capture_pane_ansi(w: &Window) -> String` (next to `capture_pane_text`,
main.rs ~1919): walk `term.renderable_content()` cells and emit a full-screen
repaint: `\x1b[2J\x1b[H`, then per cell emit SGR for fg/bg/flags (only when the
style changes from the previous cell — keep it compact), chars, `\r\n` per row,
and finally position + show/hide the cursor to match the source.

Sharer streaming loop (one thread per viewer):
- every ~100ms, capture frame; compare a hash (e.g. std DefaultHasher) of the
  seq to the last sent; send PaneFrame only when changed.
- if the shared pane died or was killed / share stopped → send ShareEnd, close.
- socket write error → drop viewer silently.

Bandwidth: ~140×50 styled cells ≈ tens of KB worst case at ≤10fps on change,
over a tailnet — fine for MVP. (Deltas can come later; wire already carries
full frames so it's a server-side-only optimization.)

## Receiver: remote panes

`Window` (main.rs:690) currently owns `master`/`writer`/`child` PTY handles.
Introduce a backend split so a pane can be either local-PTY or remote:

```rust
enum PaneBackend {
    Pty { master: Box<dyn MasterPty+Send>, writer: Box<dyn Write+Send>, child: Box<dyn Child+Send+Sync> },
    Remote { stop: Arc<AtomicBool>, from: String },
}
```

- Rendering needs NO changes: `PtyWidget` reads only `vt.term`.
- Remote pane creation: spawn a reader thread that owns the TCP stream, reads
  Wire lines, and feeds `PaneFrame.seq` bytes into `vt` via
  `parser.advance(term, seq.as_bytes())`, setting the global dirty flag —
  exactly like the PTY reader thread (main.rs:744–762).
- The remote pane's Term is sized to the SHARER's rows/cols (from
  ShareAccepted/PaneFrame); local resize does not resize the source —
  `Window::resize` and `sync_sizes` must skip Remote panes' PTY resize (still
  fine to letterbox/clip in rendering, which PtyWidget already handles).
- `send_keys` / any write path → `Response::Error { "pane is a read-only
  shared view" }` for Remote panes.
- `poll_alive`: Remote pane is alive until ShareEnd/socket close → reader
  thread sets a shared `alive`/stop flag the main loop observes (reuse the
  existing reap_dead flow).
- Session persistence: do NOT persist remote panes (skip in session save).

## Receiver: offer → accept plumbing

- `PeerEvent` gains `ShareOffer { from, title, host, token }` and the app keeps
  `pending_shares: Vec<PendingShare>` (cap ~8, newest last).
- On offer: push actionable toast (`source=from`, title=`share: <title>`,
  body includes "Ctrl-b y to accept") + penguin wave — reuse push_toast.
- Accept paths:
  - key: add `y` to the Ctrl-b prefix dispatch (handle_key, main.rs ~2673) —
    accepts newest pending share.
  - mouse: in `handle_mouse_down` (main.rs ~2617), if click lands in the
    notifications rect on a toast that maps to a pending share → accept it.
    (Track toast→share mapping via the token stored on the Toast, or match by
    source+title; keep it simple.)
  - IPC: `Request::ShareAccept { token: Option<String> }` → None = newest.
    Also `Request::SharePane { id, peer }`, `Request::ShareStop { id }`,
    `Request::ShareList` with CLI subcommands to match the UX section.
- Accept action: connect to host:9867 (1.5s timeout), send JoinShare, read
  ShareAccepted, create the remote pane, hand the socket to its reader thread.
  ShareError/timeout → error toast.

## Security / trust

- Capability token per share; only explicitly shared panes are reachable, only
  while the share is active. Sharer validates token on JoinShare.
- Trust boundary remains the tailnet (matches existing peers design). The
  share is offered to a named peer but the token is the gate.
- Read-only by default. A share is created read-only unless `--write` is
  passed, and old peers default to read-only on any missing `writable` field.

## Write access (shipped after MVP)

Viewers of a writable share can type into the sharer's PTY. Design:

- Same connection, bidirectional. The JoinShare socket — one-way in the MVP —
  now also carries viewer→sharer `Input { data }` messages and sharer→viewer
  `WriteStatus { writable }` (live grant/revoke). `writable` flags ride on
  `ShareOffer`/`ShareAccepted`, `#[serde(default)] = false` for compat.
- PTY writer stays main-thread-owned. The per-viewer stream thread spawns a
  companion input-reader that only ever emits a `PeerEvent::ViewerInput` on the
  existing mpsc; the TUI main loop maps token→pane, re-checks the writable
  flag, rate-limits, sanitizes, and writes. Peer threads never touch a PTY.
- Grant/revoke is an `Arc<AtomicBool>` shared between the sharer's
  `OutgoingShare` and the registry `ShareEntry`; flipping it is picked up by
  the stream loop within one poll and enforced at BOTH ends (input thread drops
  input when read-only; main loop re-checks to win revoke races).
- Input safety: the sharer sanitizes remote bytes down to the keyboard
  repertoire before the PTY sees them — forged bracketed-paste guards
  (`ESC[200~`/`ESC[201~`, numeric-param-aware), OSC/DCS/APC/PM/SOS strings
  (response forgery like OSC 52), and C1 control codepoints are stripped;
  allowed CSI/SS3 key sequences pass. Per-message (best-effort: write access is
  already command execution). Bounded read (no unbounded `read_line`), per-
  connection ingest rate cap, and a per-pane byte/sec budget on the main thread.
- Key encoding: frames carry a mode trailer (`DECCKM`, bracketed-paste) so the
  viewer's Term tracks the sharer's app modes and encodes arrows/keys correctly.
- Echo: the frame poll bursts from ~10 fps to ~30 fps for ~1.5s after remote
  input so keystrokes echo back quickly; no local echo/prediction (tailnet RTT
  is already imperceptible).

## Non-goals

- No multi-hop relays. No scrollback history transfer (viewer sees the live
  screen only). No delta frames. No persistence of shares across restarts.
  No per-viewer write granularity (grant/revoke is per pane, all viewers).

## Test plan

- `cargo build` clean; `cargo clippy` no new warnings.
- Unit: Wire serde round-trips for all new variants; capture_pane_ansi emits
  parseable output (feed it into a fresh alacritty Term and compare plain text
  vs capture_pane_text of the source).
- Manual loopback: two peemux instances can't easily run on one box (port 9867
  + UDS name collision) — add a hidden env override `PEEMUX_PORT` and
  `PEEMUX_SOCK` if needed for local two-instance testing, OR test the streaming
  loop against a scripted TCP client (send JoinShare, assert ShareAccepted +
  PaneFrame arrive and render).
