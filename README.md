# peemux

**The terminal for the agent era.**

A TUI multiplexer that runs a Claude conductor in the sidebar and orchestrates a wall of worker panes — claudes, shells, editors, anything.

![peemux running a wall of agents](docs/peemux.png)

---

## What it is

peemux is a terminal multiplexer like tmux, but built ground-up in Rust around one idea: **the multiplexer should be a cockpit for AI coding agents.**

- A pinned **conductor pane** in the sidebar runs `claude` and acts as your single point of contact.
- The conductor drives the rest of peemux through a standard CLI (`peemux spawn`, `peemux send-keys`, `peemux capture-pane`) using its own `Bash` tool. No API, no SDK.
- The main body holds **worker panes** — more claudes, ssh sessions, nvim, log tails, whatever — arranged in a single view or a mosaic **wall view** of live mini-renders.
- Every pane carries an **agent state dot** (🔴 blocked / 🟡 working / 🔵 done / 🟢 idle) so you can see at a glance which agents need you.

## Why it exists

tmux is great, but it doesn't know anything about agents. peemux does.

The conductor doesn't just type into other claudes — **it can drive any pane.** That means:

- It can spawn a `claude` worker, brief it, and watch its output.
- It can also `peemux capture-pane` the terminal *you* are working in — your nvim buffer, your `nmap` scan, your `tail -f` — and feed that as context to its own decisions.
- It can kill stuck panes, spin up new ones, and keep five agents working concurrently while you keep typing in your own shell next door.

It's the best of both worlds: you stay in the terminal you already love, and you get a Claude that can both **act on its own initiative** and **see what you're doing** so it can lend a hand or stay out of your way.

## Install

Requires Rust 1.76+.

```bash
git clone https://github.com/arcnid/peemux.git
cd peemux
cargo install --path .
```

Then just:

```bash
peemux
```

The server starts on first launch, the conductor pane auto-spawns `claude` with a generated `CLAUDE.md` teaching it the peemux CLI.

## Quick start

```bash
peemux                                  # attach (starts server if needed)
peemux ls                               # list panes
peemux spawn claude                     # open a new pane running claude
peemux send-keys <id> "fix the tests"   # type into a pane (auto-submits)
peemux capture-pane <id>                # dump screen contents as text
peemux kill-pane <id>                   # close a pane
peemux notify peemux "agent" "done"     # toast + voice (macOS)
peemux agent state <id> working        # push agent state
peemux agent list                       # list panes + states
peemux kill-server                      # stop the daemon
```

## Key bindings

All bindings use the `Ctrl-b` prefix (tmux-shaped, intentionally familiar).

| Keys | What it does |
|---|---|
| `Ctrl-b c` | New pane |
| `Ctrl-b n` / `p` | Next / previous pane |
| `Ctrl-b 0-9` | Jump to pane N |
| `Ctrl-b w` | Toggle single ⇄ wall view |
| `Ctrl-b Tab` | Toggle sidebar |
| `Ctrl-b o` | Cycle focus (workers ⇄ conductor) |
| `Ctrl-b &` | Close active pane |
| `Ctrl-b x` | Force-kill active pane |
| `Ctrl-b d` | Detach |
| `Ctrl-b ?` | Help |
| Mouse click (wall) | Activate tile / jump in |
| Mouse wheel | Scroll the pane under the cursor |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                       peemux server (in-proc daemon)            │
│   ┌──────────────────┐  ┌──────────────────┐  ┌──────────────┐  │
│   │ Workspace tree   │  │ PTY pool         │  │ Agent state  │  │
│   │ (ws → tab →      │  │ (one per pane,   │  │ (id, name,   │  │
│   │  pane)           │  │  alacritty VT)   │  │  task, dot)  │  │
│   └──────────────────┘  └──────────────────┘  └──────────────┘  │
│                            │                                    │
│                  Unix domain socket @ /tmp/peemux-$USER.sock    │
└────────────────────────────┼────────────────────────────────────┘
                             │
       ┌─────────────────────┼─────────────────────┐
       │                     │                     │
   ┌───▼────┐         ┌──────▼──────┐       ┌──────▼──────┐
   │ TUI    │         │ CLI         │       │ Conductor   │
   │ client │         │ subcmds     │       │ (claude in  │
   │ (you)  │         │ (spawn,     │       │  sidebar)   │
   │        │         │  send-keys, │       │             │
   │        │         │  capture)   │       │             │
   └────────┘         └─────────────┘       └─────────────┘
```

**Single binary, three roles:**
- `peemux` (no args) → start server if needed, attach TUI client
- `peemux <subcommand>` → connect to server, send command, exit
- `peemux server` → run as detached daemon (used internally)

The TUI process *is* the server — the daemon and the client live in the same process, talking to siblings over the UDS. This keeps the architecture honest while still letting any shell script or agent script peemux from the outside.

## Stack

| Layer | Crate |
|---|---|
| TUI | `ratatui` |
| Input + raw mode | `crossterm` |
| PTY spawn / IO | `portable-pty` |
| VT parsing | `alacritty_terminal` (same engine zellij uses) |
| Async runtime | `tokio` |
| IPC | `interprocess` (UDS / named pipe) |
| CLI | `clap` derive |
| Wire format | newline-delimited JSON |

## Agent state model

Each pane optionally has an attached agent with a 4-state badge:

- 🔴 **Blocked** — needs your input
- 🟡 **Working** — making progress
- 🔵 **Done** — finished, you haven't looked yet
- 🟢 **Idle** — finished, you've acknowledged

States are pushed two ways:
1. **Explicit** — `peemux agent state <id> blocked` (for agents that opt in)
2. **Heuristic** — process name + screen-output regex (for generic agents that don't know about peemux)

The conductor sees the same dots you do, so its scheduling decisions naturally reflect which workers are stuck vs. churning.

## Status

| Milestone | Status |
|---|---|
| M0 — Skeleton | ✅ |
| M1 — Real PTYs + multi-pane | ✅ |
| M1.5 — VT engine swap (vt100 → alacritty_terminal) | ✅ |
| M2 — Server + UDS + CLI vocabulary | ✅ |
| M3 — Sidebar + conductor + agent state | ✅ |
| M3+ — Friends list (real peers) | planned |
| M4 — Polish (config file, bell route, event push) | planned |
| M5+ — Worktree integration, plugins, Windows | planned |

## Non-goals

- ❌ tmux compatibility (different goals, different binary)
- ❌ Copy-mode parity with tmux (basic select/copy only at MVP)
- ❌ Library / embeddable mode (single binary, focused use case)
- ❌ Windows (M5+)

## License

MIT — see [LICENSE](LICENSE).
