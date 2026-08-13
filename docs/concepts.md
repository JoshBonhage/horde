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

Each space is also given an accent colour when it is created, shown in the tab bar, its
sidebar dot, and the borders of its panes — so which project you are looking at is something
you see rather than something you read. See [configuration](configuration.md#project-colours).

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

### What survives what

| Event | What happens |
|---|---|
| `ctrl+b d`, or closing the terminal window | only the client ends. Every pane, process, and agent keeps running. `horde` reattaches to the same daemon. |
| `horde stop`, a crash, a reboot | the daemon goes. On next start it restores the *shape* — spaces, tabs, the split tree and its ratios, names, working directories — and panes come back as fresh shells. Agents that reported a session id are resumed. |

The daemon is started in its own session, with no controlling terminal, so the SIGHUP that
goes out when a terminal window closes cannot reach it. That is the whole reason closing the
window is safe; a daemon left in the client's process group would die with it and take every
agent along.

### Upgrading without killing anything

```sh
cargo build --release
horde upgrade                    # or --exe /path/to/new/binary
```

The running daemon hands the whole session to a new process and exits. Your panes keep
running throughout — same processes, same pids, same conversations. Use this after rebuilding
instead of `horde stop`.

It works because panes are attached to the *slave* side of their PTYs while the daemon holds
the master. Transfer the master descriptors to a successor over a Unix socket
(`SCM_RIGHTS`) and the children never learn anything changed: no signals, no restarts.
Layout, names, agent state, and each pane's visible screen travel alongside as ordinary
serialised data.

The ordering is where the safety lives:

```
1. pause every reader, and wait for each to acknowledge
2. snapshot state, duplicate the PTY masters
3. spawn the successor in import mode, joined by a socketpair
4. send manifest + descriptors
5. successor rebuilds the panes and binds <socket>.handoff   -> "R"
6. we unlink <socket>                                        -> "G"
7. successor renames <socket>.handoff to <socket>            -> "B"
8. we exit without signalling any pane process group
```

The invariant is that **exactly one process may read a given PTY at a time** — two readers on
one master would tear the output stream in half. That is why step 1 waits for acknowledgement
rather than assuming.

Anything failing before step 6 rolls back: readers resume, the successor is killed, and the
session carries on untouched. Steps 6 and 7 are an unlink followed by a rename of a file the
successor already created, so the committed window has no failure mode worth planning around.

Two honest caveats:

- **Alternate-screen programs come back approximate.** `nvim`, `htop` and friends survive
  perfectly — their processes are untouched — but horde replays the visible grid, not their
  internal state, so they look off until they next redraw.
- **This is not checkpoint/restore.** It does not survive a reboot, move processes between
  machines, or preserve anything beyond the descriptors and the screen.

### What it costs to leave running

Closing the terminal does **not** free anything. That is the point — but it means you should
know the bill. Measured on this machine, detached, completely idle:

| | CPU | Resident memory |
|---|---|---|
| the daemon, 3 panes | ~0.15% of one core | 8–11 MB |
| each idle shell | ~0% | ~3.4 MB |
| each idle Claude Code | ~2% of one core | its own, and much larger |

The daemon drops to a slower cadence when no client is attached: there are no frames to
draw, so it only drains pty output and runs detection. Probing which process is in the
foreground of each pane is slower still, because that means forking `ps` and what is
*running* in a pane changes far less often than what it is *doing*.

**Your agents dominate that table, not horde.** A handful of idle Claude Code processes cost
more than the multiplexer holding them. If you left five spaces of agents running, they are
all still running.

To actually stop everything:

```sh
horde stop          # the daemon and every pane it owns
```

`horde status` tells you what is currently alive.

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

### Three names, one pane

An agent pane carries three names and none of them substitutes for another:

| | What it is | Set by |
|---|---|---|
| **name** | how you address it in `horde send` | `horde pane rename`, else the detected agent name |
| **kind** | which program is running — `claude`, `codex` | detection |
| **role** | what it is *for* — `reviewer`, `builder`, `docs` | `horde pane role`, or the right-click menu |

Only the role recurs across projects: every repo has a reviewer, and it is the same word each
time. That is what makes it the one worth grouping by, and why `horde role list` can answer
"who is reviewing, everywhere" when neither of the other two could.

A role is normalised on the way in — lowercased, spaces folded to `-`, capped at 16
characters — so `Code Reviewer` and `code_reviewer` are one role rather than three. Roles do
not have to be declared in `config.toml`; declaring one only chooses how it looks.

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
