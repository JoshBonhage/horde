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

`ask` blocks until they answer and prints the answer, so it can go straight into a variable. It
waits five minutes unless you say otherwise (`--timeout 900`); a timeout means no answer *yet*,
not that the question was lost. The recipient sees `[horde] request #42 from <you>` and is told
the exact command to reply with.

Delivery is gated on the recipient's state, which is why you never need to check first:

| Their state | What happens |
|---|---|
| `idle`, `done` | delivered now |
| `working` | queued, delivered when they reach their prompt |
| `blocked` | queued — they are at a permission prompt and a newline would answer it |

`queued` is a normal result, not a failure. Do not resend, and never route around the bus by
writing into another pane's tty.

Two refusals mean stop rather than retry:

- **"already has N messages waiting"** — twenty is the ceiling, and it exists because each
  message costs them a turn. More will not help. Put what you need on the board, or tell your
  human that pane is buried.
- **"no agent or pane called …"** — the name is wrong or they have gone. Check `horde roster`
  rather than guessing at variants.

Queued messages arrive one per turn, so keep each one whole and self-contained: you are writing
something that lands at a prompt, possibly an hour from now, without the context you have.

## The board

Work sits on the board and whoever is free takes it. Use it instead of pushing at a named
agent whenever the work does not have to be done by someone specific.

```bash
horde task add "write tests for src/bus.rs"     # onto this project's board
horde task work                                 # enlist: I will take board work
horde task claim                                # take the oldest open one
horde task done --result "18 tests added, all passing"
horde task release 4                            # put it back, someone else can take it
horde task list                                 # this project's board
horde task clear                                # drop every open task here
```

**Put work back rather than sitting on it.** `horde task release <n>` returns a task you claimed
to the board — the right move when you are blocked on something outside your reach, or about to
run out. A claimed task nobody is working is invisible: it looks handled and is not.

Write `--result` for someone who was not watching. Some board work comes from your human's own
board, where your result is filed against the card they wrote — "done" tells them nothing they
did not already assume.

Four things to know:

- **The board can be closed.** If `agents.board = false`, every one of these is refused and the
  error names the setting. That is a deliberate choice by your human, not a fault — messaging
  still works, so use `horde send` and say what you need. Do not try to route around it.
- **You are not offered work until you enlist.** `horde task work` once, then you may be told
  when tasks are waiting. Without it nothing will ever interrupt you, which is deliberate: an
  agent someone opened to think with is not a worker.
- **A claim is exclusive.** Two agents claiming at the same moment get different tasks, never
  the same one. Losing the race is a visible error, not a silent duplicate.
- **A claim is filtered by your role.** `horde task claim` returns work for *you* — general work,
  plus anything tagged with the role you were given. A reviewer's task will not be handed to you
  because you happened to be free, so an empty board can mean "nothing for you" while somebody
  else has plenty. `horde task list` shows the whole board and who each task is for.

You cannot change your own role. It decides what work reaches you, so it is given at spawn or by
your human — `horde pane role` from an agent is refused. If you have the wrong one, say so.

## Work for a role

A task may name the role that takes it:

```bash
horde task add "review the parser diff" --role reviewer
horde task add "port the old parser" --role builder --space api
horde task add "update the changelog"                  # anyone free
```

Tag work when it needs a particular kind of agent, and leave it untagged when it does not —
untagged work goes to whoever is free, which is usually what you want and always the cheaper
option.

Adding a task for a role nobody has prints a note saying so. Believe it: that work will sit
untouched until such an agent exists, because nothing is ever offered to an agent that could not
claim it. Either start one (`horde spawn --role reviewer --board`) or leave the task untagged.

Where your human has set `agents.task_authors`, only certain roles may add tasks at all, and
everyone else is refused with the list. That is a deliberate shape — work is proposed to whoever
leads the project rather than written by whoever thought of it — so `horde send` them what you
found instead of routing around it.

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
| `--role` | what it is *for*: reviewer, builder, docs. Makes a fleet readable, and decides what board work it is offered |
| `--worktree` | its own git worktree and branch beside the project. Only when asked |
| `--board` | enlist it for board work in this project |
| `--task` | a first job, put on the board tagged with its role, for it to claim |
| `--cmd` | the whole command, so the model goes here: `"claude --model opus"` |
| `--profile` | start on a named model list from config instead of `--cmd` — see below |
| `--brief` | its first instruction, delivered once it is up. Works with the board closed |

`--role` is load-bearing when the board is in use, not just a label: it is what makes `--task`
that agent's task rather than the next thing any idle pane picks up. Pass it whenever you pass
`--task`.

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

### If you are leading a project

You may be the agent your human plans through — often the only role allowed to write to the
board. If so, the work is turning something vague into work someone else can pick up, and then
getting out of the way:

```bash
# One feature, broken into work that names who does it and cannot collide.
horde spawn --name parser   --role builder  --worktree --board
horde spawn --name reviewer --role reviewer --worktree --board
horde task add "port the tokenizer to the new AST — tests in tests/parse.rs" --role builder
horde task add "review the tokenizer port once it lands"                     --role reviewer
```

Four things that make the difference between a fleet and a mess:

- **A worktree each, always.** Two agents in one working tree is not a merge conflict, it is one
  agent's work silently gone. `--worktree` is the whole protection and it costs nothing.
- **Tag the work, then leave it alone.** Do not also `horde send` the same instruction: the agent
  is nudged when work it can claim exists, and being told twice costs it a turn deciding whether
  the two messages are the same job.
- **Write a task someone can start from.** The agent claiming it has your context and none of
  your conversation. Name files, name the test command, say what done looks like.
- **You are not the dispatcher.** horde decides who runs next — it knows who is enlisted, who is
  holding something, who has been idle longest, and which roles can take what. Watching the
  roster and handing work out by name is slower, and it stops entirely the moment you are busy.

Read `horde task list` to see where things stand, and `horde digest --keep` to catch up without
consuming your human's copy.

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
