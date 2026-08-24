---
name: horde
description: "Work alongside other AI agents running in horde, a terminal multiplexer. Use this skill to message another agent, ask one a question and get the answer back, take work from the shared board, build a fleet of agents for a project (each in its own git worktree), see what the others are doing, or catch up on a session. Requires HORDE_PANE to be set."
---

# horde

horde runs several coding agents side by side in one terminal. You are one of those agents.

First, confirm you are inside horde:

```bash
test -n "$HORDE_PANE"
```

If that fails, say you are not running inside horde and stop.

## Everything is scoped to a project

horde calls a project a **space**. Every command below acts on the space your pane is in
unless you say otherwise. This matters more than it sounds: the board is per project, so work
you add is offered only to agents in *this* project, and `horde task list` shows only this
project's board. If you want another project's, say `--everywhere` or name it.

## Talking to another agent

```bash
horde roster                                   # who is here, and what state they are in
horde send reviewer "the parser lands in src/parse.rs"
answer=$(horde ask reviewer "does src/bus.rs handle a dropped pane?")
horde reply 42 "yes — it queues and flushes on the next idle pass"
horde broadcast "I am taking the parser work"
```

`ask` blocks until they answer and prints the answer, so it can go straight into a variable.
The recipient sees `[horde] request #42 from <you>` and is told the exact command to reply
with.

Delivery is gated on the recipient's state, which is why you never need to check first:

| Their state | What happens |
|---|---|
| `idle`, `done` | delivered now |
| `working` | queued, delivered when they reach their prompt |
| `blocked` | queued — they are at a permission prompt and a newline would answer it |

`queued` is a normal result, not a failure. Do not resend, and never route around the bus by
writing into another pane's tty.

## The board

Work sits on the board and whoever is free takes it. Use it instead of pushing at a named
agent whenever the work does not have to be done by someone specific.

```bash
horde task add "write tests for src/bus.rs"     # onto this project's board
horde task work                                 # enlist: I will take board work
horde task claim                                # take the oldest open one
horde task done --result "18 tests added, all passing"
horde task list                                 # this project's board
horde task clear                                # drop every open task here
```

Three things to know:

- **The board can be closed.** If `agents.board = false`, every one of these is refused and the
  error names the setting. That is a deliberate choice by your human, not a fault — messaging
  still works, so use `horde send` and say what you need. Do not try to route around it.
- **You are not offered work until you enlist.** `horde task work` once, then you may be told
  when tasks are waiting. Without it nothing will ever interrupt you, which is deliberate: an
  agent someone opened to think with is not a worker.
- **A claim is exclusive.** Two agents claiming at the same moment get different tasks, never
  the same one. Losing the race is a visible error, not a silent duplicate.

The worker loop, when you have been told to work the board:

```bash
horde task work
while work=$(horde task claim); [ -n "$work" ]; do
  # do $work as a normal turn — think, edit, test
  horde task done --result "<what happened>"
done
```

An empty board exits 0 and prints nothing, so the loop ends cleanly. "No work" and "broken"
are different things.

## Building a fleet

You can start other agents. Each flag matters when you are standing up more than one:

```bash
horde spawn --cmd "claude --model opus" --name parser \
  --role builder --worktree --board --task "port the old parser"
```

| Flag | What it does |
|---|---|
| `--name` | how you address it in `horde send`. Always pass it |
| `--role` | what it is *for*: reviewer, builder, docs. Makes a fleet readable |
| `--worktree` | its own git worktree and branch beside the project. Only when asked |
| `--board` | enlist it for board work in this project |
| `--task` | a first job, put on the board for it to claim |
| `--cmd` | the whole command, so the model goes here: `"claude --model opus"` |
| `--profile` | start on a named model list from config instead of `--cmd` — see below |
| `--brief` | its first instruction, delivered once it is up. Works with the board closed |

**`--worktree` only when your human asks for it.** It is the right answer when several agents
will edit the same repository — without it they share one working tree, and two agents editing
the same file is not a merge conflict you can resolve, it is one agent's work silently
overwritten. But it is also a directory on their disk and a branch in their repository, and
neither is yours to create uninvited. If you think a fleet needs isolating, say so and let them
decide.

When you are asked for one, the worktree lands *beside* the project — an agent named `ads` on
`~/dev/WCP` works in `~/dev/WCP-ads`, on branch `horde/ads`.

There is a cap on how many panes agents may have open at once (`agents.max_fleet`, six by
default). Hitting it is an error that says so. Do not work around it; tell your human.

Brief each agent as you spawn it with `--brief`. Do not `horde send` to a pane you just made:
for a second or two it has no agent yet, so the message is typed into a still-booting TUI
instead of being queued. `--brief` waits for it to come up.

After that, `horde send` works normally, or put the work on the board and let them claim it. The board is better for more than two agents: it self-balances, and you never have
to track who is free.

## Models

Your human may have defined named model lists in config. Each is a command plus an ordered set
of models to work through:

```bash
horde spawn --profile free --name helper     # starts on the first model in the "free" list
```

Use a profile when you want *a* worker and do not care which model, and `--cmd` when the model
matters. An unknown profile name is refused and the error lists the real ones — it never quietly
falls back to a different model, because that would spend a budget nobody chose.

## Watching

```bash
horde roster --json                                      # states, for deciding what to do next
horde pane read reviewer --source detection --lines 40   # what another agent is looking at
horde wait reviewer --until idle --timeout 300           # idle · done · blocked · working · serving
horde bus tail                                           # the message log
horde digest --keep                                      # what has happened; --keep leaves it unread for your human
```

States are `working`, `blocked`, `done`, `idle`, `unknown`, `serving`. Two need explaining:

- **`blocked` means a human is needed.** The agent is at a permission prompt. Nothing you can
  send reaches it, and messages you send are held until it is answered.
- **`serving` is not an agent.** It is a dev server or watcher, and there is nobody in there
  to talk to.

## If you are running out

If you hit your own usage limit, say so before you stop. Your human may not be watching, and a
pane that goes quiet looks the same as one that is thinking:

```bash
horde broadcast "hit my usage limit — parser work is half done, tests not run yet"
```

Leave the working tree in a state someone else can pick up: commit or stash, and say which.
A successor that inherits a dirty tree with no note has to guess what you meant.

Full reference: `horde docs orchestration`.
