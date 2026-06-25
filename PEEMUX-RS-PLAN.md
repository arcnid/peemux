# peemux-rs — the plan

Ground-up Rust rewrite of peemux. The C/tmux-fork prototype at `~/peemux`
proved the IPC contract (`peemux <subcommand>` shell-out, no custom socket
required of callers) and the visible chrome ceiling (tmux's mouse layer is
bolted on; mouse-native feel needs end-to-end frame ownership). This crate
owns the frame end-to-end.

## Goal

Be the **TUI cockpit for AI coding agents**. Five concurrent claudes in 30
seconds. See every agent's state at a glance. Wall view to scan all of them
at once. Voice notify when one needs you. Talk to fiveHead-core (and any
other orchestrator) over a stable shell-out CLI + UDS protocol.

## Stack

| Layer | Crate | Why |
|---|---|---|
| TUI render | `ratatui` | Best-in-class, used by herdr/zellij/atuin |
| Input + raw mode | `crossterm` | Cross-platform, async event stream |
| PTY spawn / io | `portable-pty` | wezterm's PTY crate, vendored by herdr |
| VT parsing | `vt100` (MVP) → `wezterm-term` or libghostty (later) | Get to a runnable demo fast; swap engine when correctness matters |
| Async runtime | `tokio` | PTY I/O + socket + UI events |
| IPC | `interprocess` | UDS on unix, named pipe on Windows |
| CLI | `clap` derive | Subcommand parsing |
| Wire format | `serde_json` (MVP) | Human-readable while iterating; switch to bincode if perf matters |
| Logging | `tracing` | Structured |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                       peemux server (daemon)                    │
│   ┌──────────────────┐  ┌──────────────────┐  ┌──────────────┐ │
│   │ Workspace tree   │  │ PTY pool         │  │ Agent state  │ │
│   │ (ws → tab →      │  │ (one per pane,   │  │ (id, name,   │ │
│   │  pane)           │  │  vt100 parser)   │  │  task, dot)  │ │
│   └──────────────────┘  └──────────────────┘  └──────────────┘ │
│                            │                                    │
│                  Unix domain socket (interprocess)              │
└────────────────────────────┼────────────────────────────────────┘
                             │
       ┌─────────────────────┼─────────────────────┐
       │                     │                     │
   ┌───▼────┐         ┌──────▼──────┐       ┌──────▼──────┐
   │ TUI    │         │ CLI         │       │ Helpers     │
   │ client │         │ subcmds     │       │ (fiveHead,  │
   │ (you)  │         │ (spawn,     │       │  agents,    │
   │        │         │  agent,     │       │  scripts)   │
   │        │         │  notify)    │       │             │
   └────────┘         └─────────────┘       └─────────────┘
```

**Single binary**, three roles:
- `peemux` (no args) → start server if needed, attach TUI client
- `peemux <subcommand>` → connect to server, send command, exit
- `peemux server` (internal) → run as detached daemon

## Workspace model (4-level, herdr-inspired)

```
Server
└── Workspace    e.g. "steel-ai", "scafco", "peemux"
    └── Tab      e.g. "frontend", "backend", "scratch"
        └── Pane e.g. one PTY running `claude`, with agent state
```

The extra "workspace" layer above tmux's session/window/pane is what lets
the sidebar group agents by project — when you're running 5 claudes across
3 projects, you want them grouped.

## Agent state model (herdr-inspired vocab)

Each pane optionally has an attached agent with a 4-state badge:

- 🔴 **Blocked** — needs your input
- 🟡 **Working** — making progress
- 🔵 **Done (unseen)** — finished, you haven't looked yet
- 🟢 **Idle (seen)** — finished, you've acknowledged

Pushed via two paths:
1. **Explicit** — `peemux agent state <pane-id> blocked` (for fiveHead-core
   and opt-in agents)
2. **Heuristic** — process name + screen-output regex (for generic CLI
   agents that don't know about peemux)

## Views

- **Single** (default) — focused pane fills the body, sidebar on left,
  status bar on bottom. Like vanilla terminal, but with chrome.
- **Wall** — mosaic of every active pane tiled across the screen, each
  with a live mini-render of its current screen. Mouse-click a tile to
  jump in. **This is the peemux differentiator** — herdr only has a
  sidebar list.

## Conductor architecture (the AI layer)

This is what makes peemux distinct from tmux: peemux stays a dumb
multiplexer, and a **conductor Claude Code** in a pinned sidebar pane is
the brain. The conductor is the user's main point of contact — natural
language in, orchestrated agent panes out.

```
┌──────────────┬───────────────────────────────────────────────────┐
│   sidebar    │             main body (single OR wall)            │
│              │                                                   │
│ ┌──────────┐ │   ┌─────────────────┐  ┌──────────────────────┐   │
│ │ friends  │ │   │ worker pane 1   │  │ worker pane 2        │   │
│ │  list    │ │   │ claude on       │  │ claude on            │   │
│ │ ━━━━━━━━ │ │   │  fix-tests      │  │  ship-feature        │   │
│ │ notif    │ │   │                 │  │                      │   │
│ │ banner   │ │   └─────────────────┘  └──────────────────────┘   │
│ │ ━━━━━━━━ │ │   ┌─────────────────┐  ┌──────────────────────┐   │
│ │          │ │   │ worker pane 3   │  │ worker pane 4        │   │
│ │ ┏━━━━━━┓ │ │   │  ssh prod       │  │ tail -f logs         │   │
│ │ ┃conduc┃ │ │   │                 │  │                      │   │
│ │ ┃ claud┃ │ │   └─────────────────┘  └──────────────────────┘   │
│ │ ┗━━━━━━┛ │ │                                                   │
│ └──────────┘ │                                                   │
└──────────────┴───────────────────────────────────────────────────┘
```

**Key idea:** the conductor doesn't need an API or SDK. It's a regular
`claude` invocation in a peemux pane, and it drives the rest of peemux
through the standard CLI surface using its existing `Bash` tool.

**User flow:**
1. User types into the conductor sidebar: *"spin up two agents to work
   on the test suite and the docs"*
2. Conductor runs `peemux spawn claude` twice, gets back pane-ids
3. Conductor uses `peemux send-keys <id> "fix the failing tests in
   src/"` to brief each worker
4. Conductor optionally `peemux capture-pane <id>` to read worker
   output and report status back to the user

**Sharing `~/.claude/` across instances:** confirmed working in tmux —
Kaleb runs multiple concurrent `claude` sessions today, no auth
interleaving issues. Doesn't need special handling.

**Sidebar real estate:** the sidebar holds three stacked widgets:
- **friends list** — other peemux/fiveHead/agent peers (M3+)
- **notification banner** — toast strip for `peemux notify` events
- **conductor pane** — pinned `claude` PTY (this is the chat surface)

**Wall and single view still work the same.** The conductor sidebar is
chrome — you can still click into any worker pane, type into it
directly, switch to wall view to scan all of them at once. The AI layer
is *additive*, not replacing the multiplexer underneath.

## Commands (CLI surface — the conductor's vocabulary)

These are exactly what the conductor Claude needs to drive peemux from
its Bash tool. Designed to be tmux-shaped so it's already familiar.

| Command | What it does | Status |
|---|---|---|
| `peemux` | Start server if needed, attach TUI client | M0 |
| `peemux server` | Run server in foreground (detached normally) | M2 |
| `peemux kill-server` | Stop the server | M2 |
| `peemux ls` | List panes: id, title, agent state | M2 |
| `peemux spawn [cmd]` | Open new pane, launch cmd; prints pane-id | M2 |
| `peemux send-keys <pane-id> <text>` | Type into a pane | M2 |
| `peemux capture-pane <pane-id>` | Dump screen contents as text | M2 |
| `peemux kill-pane <pane-id>` | Close a pane | M2 |
| `peemux notify <src> <title> <body>` | Toast + optional voice | M2 |
| `peemux agent state <pane-id> <state>` | Push agent state | M3 |
| `peemux agent list` | List all agents and their states | M3 |

## Milestones

### M0 — Skeleton ✅
- Cargo scaffold + dep stack
- `peemux` runs, opens TUI with branded chrome
- View toggle stub

### M1 — Real PTYs + multi-pane ✅ (current)
- PTYs embedded via `portable-pty`, parser is `vt100`
- Ctrl-b prefix, c/n/p/0-9/&/x/w/?/d bindings, mouse capture
- Single + Wall views with click-to-activate and arrow-key spatial move
- Per-tile resize so wall tiles scroll-follow correctly
- Auto-reap on `exit`/Ctrl-D, force-kill via `Ctrl-b x`
- Mouse wheel → arrow keys to the pane under the cursor

### M1.5 — VT engine swap ✅
- Replaced `vt100` with `alacritty_terminal` 0.26 (same engine zellij uses)
- Unlocks btop, spf, vim graphics, embedded Claude Code in the conductor seat

### M2 — Server + UDS + conductor's CLI vocabulary ✅
- In-process UDS server at `/tmp/peemux-$USER.sock`, newline-delimited JSON
- Shipped: `peemux ls`, `spawn`, `send-keys [--enter]`, `capture-pane`,
  `kill-pane`, `notify`, `kill-server`
- Voice hook on notify (macOS `say`)
- Scripts peemux from *anything* — not just the conductor

### M3 — Sidebar + conductor + agent state ✅
- `Ctrl-b Tab` toggles the sidebar; `Ctrl-b o` cycles focus
- Sidebar layout: PEEMUX badge + friends placeholder + notification banner
  + pinned conductor PTY
- Conductor auto-spawns `claude` with a generated CLAUDE.md in a per-session
  tmpdir teaching it the peemux CLI vocabulary
- Notification banner shows recent `peemux notify` events
- 4-state agent dots on tab strip + wall tiles (Blocked / Working / Done / Idle)
- Heuristic detection: blocked/working pattern scan over agent panes (1Hz)
- `peemux agent state <id> <state>` and `peemux agent list` CLI

### M3+ — Friends list
- Wire the placeholder to real peers (fiveHead-core, agent peers)

### M4 — Polish
- Bell auto-route
- Durdraw splash/idle anim
- Config file at `~/.config/peemux/config.toml`
- Crash-survive (panes outlive server restart)
- Event push back to conductor (so it doesn't have to poll
  `capture-pane`)

### M5+ — Differentiation
- Worktree integration (per fiveHead workflow)
- Plugins / scriptable hooks
- Windows port

## Non-goals (for now)

- ❌ tmux compatibility — different goals, different binary
- ❌ Copy-mode parity with tmux — basic select/copy only at MVP
- ❌ Library / embeddable mode — single binary, focused use case
- ❌ Plugin marketplace — script hooks are enough
- ❌ Windows (M5+)

## Honest cost estimate

The "skeleton that runs Claude in a pane with a sidebar dot" milestone is
realistically 2–4 weeks of focused work. The peemux-original wall view
adds another 1–2 weeks (live mini-renders are non-trivial — likely scaled
snapshots refreshed at low Hz, not real 60fps tiles).

The C-fork peemux at `~/peemux` is preserved as `peemux-classic` — it
works, `peemux-notify` is shipped, and it stays available as a fallback
multiplexer while peemux-rs catches up.
