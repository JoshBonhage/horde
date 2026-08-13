---
name: horde
description: "Work alongside other AI agents running in horde, a terminal multiplexer. Agent-to-agent messaging (the bus) and the shared task board are both PAUSED — nothing can be sent, claimed, or delegated, and no work will arrive from another agent. Use this skill to find out what the other agents are doing, read another agent's output, wait for one to finish, start a helper agent, or catch up on a session — and to understand why a `horde send`/`task claim` command was refused. Requires HORDE_PANE to be set."
---

# horde

horde runs several coding agents side by side in one terminal. You are one of those agents.

First, confirm you are inside horde:

```bash
test -n "$HORDE_PANE"
```

If that fails, say you are not running inside horde and stop.

## What is switched off

**The message bus and the task board are both paused while they are reworked.** This is
deliberate, not a fault, and there is nothing to diagnose or work around:

| Command | Now |
|---|---|
| `horde send` · `horde ask` · `horde reply` · `horde broadcast` | refused, exits non-zero |
| `horde task add` · `claim` · `done` · `release` | refused, exits non-zero |
| `horde bus tail` · `horde task list` | still work, read-only |

What follows from that:

- **Nothing will arrive from another agent.** No `[horde]` lines, no nudges about work waiting
  on the board. Your work comes from the human in your pane. Do not poll for it, and do not
  treat a quiet board as "nothing to do".
- **You cannot delegate or ask.** If a task needs another agent, say so to your human and let
  them decide — do not try to route around it with `tmux send-keys`, by writing into another
  pane's tty, or by any other back door. The pause is the point.
- **If one of these commands is refused, that is the expected result.** Report it plainly and
  move on; do not retry it, and do not go looking for a bug.

## What still works

Observing the other agents is untouched — none of it puts text into anyone's pane.

```bash
horde roster                                      # names and states
horde roster --json                               # the same, for deciding what to do next
horde pane read reviewer --source detection --lines 40   # what another agent is doing
horde wait reviewer --until idle --timeout 300    # idle · done · blocked · working
horde bus tail                                    # the message log, read-only
horde task list                                   # what was left on the board
```

States are `working`, `blocked`, `done`, `idle`, `unknown`. `blocked` means a **human** is
needed — the agent is sitting at a permission prompt, and nothing you can do reaches it.

## Starting a helper

Still available: it starts a new program rather than typing at one already running.

```bash
horde spawn --cmd claude --name tester --split right
```

Pass `--name` so the pane is addressable in the roster. Note that with the bus paused you
cannot brief the agent you just started — it comes up at an empty prompt and waits for the
human. Prefer telling your human what you would have spawned.

## Catching up

If you were restarted, or you are picking up a session someone else was driving:

```bash
horde digest --keep
```

Tells you which agents need a human and what has happened. Use `--keep`; the digest belongs to
your human, not to you.

Full reference: `horde docs orchestration`.
