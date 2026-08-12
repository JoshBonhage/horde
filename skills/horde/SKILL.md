---
name: horde
description: "Coordinate with other AI agents running alongside you in horde, a terminal multiplexer with a message bus. Use when you need to delegate work to another agent, answer a request from one, check what other agents are doing, or start a helper agent. Also use whenever you see a line beginning with [horde] in your input. Requires HORDE_PANE to be set."
---

# horde

horde runs several coding agents side by side and gives them an addressable way to talk to
each other. You are one of those agents.

First, confirm you are inside horde:

```bash
test -n "$HORDE_PANE"
```

If that fails, say you are not running inside horde and stop.

## Messages arrive as if the human typed them

Anything that reaches you starting with `[horde]` came from **another agent**, not from your
human. Three forms:

| What you see | What it means | What to do |
|---|---|---|
| `[horde] message from X: ...` | one-way message | act on it; no reply expected |
| `[horde] request #N from X: ...` | X is **blocked waiting for you** | do the work, then run `horde reply N "<answer>"` |
| `[horde] reply from X (re #N): ...` | the answer to something you asked | continue with it |

**A request is the important case.** The sender is sitting in a blocking call until you
answer. Do not investigate whether `horde reply` exists — it is a real command on your PATH.
Run it directly, once, with your answer as a single quoted argument:

```bash
horde reply 42 "the gating logic is sound; queued messages flush on the next idle pass"
```

Keep the answer to one line. It is delivered as a single message.

## Asking another agent

```bash
horde ask reviewer "does src/bus.rs handle a dropped pane?"
```

This **blocks** until they answer, then prints their answer on stdout — so capture it:

```bash
verdict=$(horde ask reviewer "is this safe?")
```

Use `--timeout <seconds>` (default 300) if the work is long. Prefer `ask` over `send` whenever
you actually need the answer; `send` gives you no way to know if anything happened.

## One-way messages

```bash
horde send reviewer "schema is applied, please review src/db/"
horde broadcast "pausing for a deploy, hold off on migrations"
```

Phrase these as instructions, not chat — they land as a prompt in the recipient's session, so
"please review src/db/" works and "hey what do you think" wastes a turn.

## Shared work: the board

Work can also sit on a board for whoever is free, instead of being addressed to you:

```bash
horde task list                 # what is outstanding
work=$(horde task claim)        # take the oldest one — exclusive, no two agents get the same task
horde task done --result "18 tests added, all passing"
horde task release <id>         # hand it back if you cannot do it
```

`claim` prints `nothing on the board` and exits 0 when there is no work, so a loop can tell
empty from broken:

```bash
while work=$(horde task claim); [ -n "$work" ]; do  # do $work, then:
  horde task done --result "<what happened>"
done
```

Claim one at a time — claiming a batch starves the other agents. If your pane dies holding a
task, horde returns it to the board for you.

## Seeing who else is here

```bash
horde roster          # names and states, human-readable
horde roster --json   # the same, for deciding what to do next
```

States: `working`, `blocked`, `done`, `idle`, `unknown`. Two rules worth knowing:

- **`blocked` means a human is needed** — the agent is at a permission prompt. Messaging will
  not unblock it. Tell your human instead of working around it.
- **A message to a busy agent is queued, not lost.** horde delivers it when they reach their
  prompt. `queued` in the output is a normal result, not a failure.

## Working with another agent's output

```bash
horde pane read reviewer --source detection --lines 40
```

Cheaper than asking, when you only need to see what they are doing.

## Waiting

```bash
horde wait reviewer --until idle --timeout 300
```

`--until` takes `idle`, `done`, `blocked`, or `working`. `done` also satisfies `idle`.

## Starting a helper

```bash
horde spawn --cmd claude --name tester --split right
```

Always pass `--name`: it is how you address them afterwards. A new agent takes a few seconds
to boot; either `horde wait <name> --until idle` first, or just send and let the queue hold it.

## Rules of the road

- You never say who you are. `HORDE_PANE` is in your environment, so `horde send` and
  `horde reply` are automatically attributed to you.
- You cannot message yourself.
- One message is one turn of work for the recipient. Batch rather than sending five in a row.
- If you need an answer, use `ask` — or, with `send`, state the exact `horde reply` or
  `horde send` command you want run back. There is no implicit reply channel.

Full reference: `horde docs orchestration`.
