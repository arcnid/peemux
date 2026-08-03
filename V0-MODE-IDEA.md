# Idea: v0 Mode — peemux as v0.dev for the console

## One-liner

Keep the conductor exactly as it is — you talk, it orchestrates. But instead of
worker panes being raw shells you have to read like a sysadmin, the other peemux
windows become **live views of the project**: what v0.dev's preview panel is to
its chat, the pane grid is to the conductor.

## Why

Today the loop is: prompt conductor → agents churn in panes → you alt-tab to a
browser to see if the site actually changed. The feedback lives outside the
multiplexer. v0's core trick isn't codegen, it's that the artifact is always in
view while you iterate on it. peemux already owns the screen real estate —
the panes just render the wrong thing (scrollback instead of state).

## UX sketch

```
┌────────────┬──────────────────────────┬────────────────┐
│  sidebar   │  VIEW: preview           │  VIEW: diff    │
│  (friends, │  live render of the app  │  files changed │
│  penguin,  │  (localhost:3000)        │  this session  │
│  states)   ├──────────────────────────┼────────────────┤
│            │  VIEW: agent activity    │  VIEW: routes/ │
│  conductor │  what each worker is     │  components    │
│  input ↓   │  doing, one line each    │  map of the    │
│  ❯ _       │                          │  project       │
└────────────┴──────────────────────────┴────────────────┘
```

- **Conductor stays the same.** Same prompt box, same role, same CLI surface.
- Worker agents still exist and still run in real ptys — they're just not the
  thing you look at by default. Views are.
- A view pane is read-only and auto-refreshing. Clicking/focusing one can drill
  in (preview → element inspect, diff → full file, activity → attach to the
  worker's pty like today).

## Candidate view types

| View       | Renders                                        | Source                              |
|------------|------------------------------------------------|-------------------------------------|
| `preview`  | the running app, refreshed on file change      | headless browser screenshot → sixel/kitty graphics, or textual DOM outline as fallback |
| `diff`     | working-tree diff, grouped by session/agent    | `git diff` + fs watcher             |
| `activity` | one status line per worker (state, last action)| existing agent-state heuristic      |
| `routes`   | route/component tree of the project            | framework adapter (Next.js first)   |
| `logs`     | dev-server output, filtered/deduplicated       | wrap the dev server pty             |

## Implementation notes (rust side)

- New pane kind alongside the pty pane: `ViewPane { kind, refresh, source }`.
  Views don't own a shell; they own a producer task that writes frames.
- CLI: `peemux view spawn preview --url localhost:3000`,
  `peemux view spawn diff`, etc. Conductor can call these like it calls
  `spawn` today — so the conductor can *compose the cockpit* for the task at
  hand ("working on the gallery → open preview scrolled to #gallery + diff").
- Preview rendering is the hard one. Tiered approach:
  1. kitty/iTerm2 graphics protocol → real screenshots (headless chromium).
  2. sixel fallback.
  3. pure-text fallback: DOM outline / lighthouse-style summary.
- File watching: one global watcher, views subscribe. Screenshot debounce
  ~300ms after last write so agent edit-bursts don't thrash chromium.
- Agent hooks: when a worker finishes a task, conductor already knows → it can
  flash the relevant view (penguin points at what changed?).

## What this is not

- Not a browser replacement — it's a *glanceable* render so you stay in flow;
  final review still happens in a real browser.
- Not a new conductor. No behavior change to orchestration, send-keys, or
  peer messaging.

## Open questions

- Terminal graphics support detection/negotiation (kitty vs sixel vs text) —
  how degraded is the text fallback before the feature stops being worth it?
- One chromium instance per preview view or a shared pool?
- Do views live in the layout tree like panes (splittable/resizable) — probably
  yes, reuse everything — or a separate fixed "dashboard" region?
- Multi-project: views are per-cwd; what happens when workers span repos?
