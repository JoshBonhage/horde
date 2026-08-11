# Concepts

## The hierarchy

```
session
└── space          a project or task, with a working directory
    └── tab        a layout within that space
        └── pane   a real terminal
```

**Space** — one per repo, task, or investigation. Has a working directory that new panes
inherit. Addressable by name in `horde send` and `horde space focus`.

**Tab** — a layout inside a space. Use them to separate views: `agents`, `logs`, `review`.

**Pane** — a real PTY. Panes tile edge-to-edge in a binary space partition tree and each
draws its own border, so adjacent panes show touching borders. That is intended, not an
artifact.

## The daemon

horde is a background daemon plus a thin client.

```
horde daemon   owns every PTY, every terminal emulator, all layout and geometry
horde          attaches as a client: draws frames, forwards keystrokes, owns nothing
```

The client can die, be killed, or lose its terminal without disturbing a single running
process. That is the whole point: close your laptop and your agents keep working.

**The daemon owns terminal emulation, not the client.** This is load-bearing. Agent status
detection has to keep working while nothing is attached, so something server-side must
always be able to see each pane's screen. Each pane keeps a mirror of its visible grid, and
that mirror is the single thing everything else reads — clients diff against it, detection
matches against it, `horde pane read` returns it.

**Geometry lives in the daemon and nowhere else.** The client draws where it is told. That is
why a pane's PTY size and its drawn rectangle can never drift apart.

### One consequence worth knowing

The daemon outlives every client and **survives rebuilds**. After `cargo build`, run
`horde stop` before reattaching, or you will connect a new client to the old daemon. horde
warns when it notices a version mismatch, but stopping is the cure.

## Agents

An agent is just a program in a pane that horde recognises — `claude`, `codex`, `gemini`,
and others. horde does not launch them any differently; it watches them.

What it adds is **state**: `working`, `blocked`, `done`, `idle`, `unknown`. See
[agents](agents.md) for how that is determined and [orchestration](orchestration.md) for
what you can do with it.

`done` deserves a note: it means *finished while you were not looking*. It clears when you
look at the pane. That is what makes the sidebar worth glancing at — it distinguishes "still
thinking" from "waiting for you since ten minutes ago".

## Two channels, one socket

The daemon listens on `~/.config/horde/horde.sock`.

- **Control** — newline-delimited JSON. Every `horde <noun> <verb>` command is one call.
  Human-readable on purpose, so it can be debugged with `nc` and scripted from anything.
- **Render** — a connection that sends `attach` switches to length-prefixed binary frames
  carrying only the rows that changed, run-length encoded by style.

Agents only ever use the control channel. See [socket-api](socket-api.md).
