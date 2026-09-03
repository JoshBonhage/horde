# Memory: what a project knows, between agents

An agent's context is finite, and every coding agent handles a full one the same way: it
summarises the conversation and throws the detail away. What survives is what the summariser
thought mattered, which is almost never the thing that took the session to work out — the dead
end you already ruled out, the reason the obvious refactor does not work, where the real entry
point is.

The usual workaround is to tell the agent to write a note first. That works, and then the note
is a file in a repository that nobody remembers is there, least of all the *next* agent, who is
a fresh process with no memory of the conversation that produced it.

A **memory** is that note, made a thing in the session rather than a thing in a directory.

```
 AGENTS
 ▾ api-refactor        ◍1 ◐1
 ├─◐ builder            2m18s
 ╰─◍ reviewer         blocked
────────────────────────────
 MEMORY                    2
 ▾ api-refactor            2
 ├─◈ api-shape         6m40s
 ╰─◈ builder-context   2h00m
────────────────────────────
 SERVICES                  3
 ...
```

Notes hang on the same coloured strand as the agents they are for, so which project a note
belongs to is something you see rather than work out — the same connector the AGENTS and
SERVICES lists use, in the project's own accent.

## Where they live

`<project>/.horde/memory/<name>.md`, beside the codebase rather than in horde's own state
directory. That is deliberate, and it is the difference between this and the task board: a
memory is knowledge about a codebase, so it belongs where it can be committed, reviewed,
diffed and read by a person with no horde running. `.horde/handoff-<name>.md` already
established the convention; this generalises it from "the agent being replaced" to "any agent
that needs it".

Notes are **per project, not per worktree**. An agent working in `.horde/worktrees/builder`
is working on the project, and a note it saves is about the project — writing it into the
worktree would put it somewhere that is deleted when the worktree is.

Whether you commit them is your call. They are ordinary markdown; `.horde/` is a reasonable
thing to gitignore and a reasonable thing to check in, and horde does neither for you.

## Writing one

```sh
horde memory save api-shape <<'EOF'
# API shape

The v2 handler splits into three, and `mod.rs` is only routing. The obvious
consolidation does not work: `Ctx` is not `Send` past the middleware boundary,
which is why the split exists at all. Already tried and reverted: 4f2a1c.
EOF

horde memory list          # newest first
horde memory show api-shape
horde memory give api-shape reviewer
horde memory rm api-shape
```

`save` reads stdin by default, so an agent can pipe a heredoc in one call rather than escaping
a paragraph onto a command line.

**What belongs in one:** the things that took the session to work out and that a fresh agent
would have to rediscover — what you ruled out and why, where the real entry points are, which
of two plausible designs the code actually uses. Not a summary of what you did; git has that.

## Handing one over

Two ways in, one command underneath.

- **Drag it.** Press a note in the sidebar, drag onto an agent's row, a project row, or the
  agent's pane itself, and release. The status bar names what you picked up and what you are
  over, so you can see you are about to hit `builder` before you let go. Escape puts it back
  down; releasing over nothing is how you change your mind.
- **Press enter on it.** With the sidebar focused (`ctrl+b e`, then `j`/`k`), enter hands the
  note to the agent you are focused on. If that is not an agent, it says so rather than
  guessing — a note in the wrong agent's context cannot be taken back out.

What the agent receives is a **path and one line of why**, never the contents:

```
Read .horde/memory/api-shape.md — saved context for this project: API shape
```

That is the whole point. A memory exists because somebody was running out of context, and
pasting the note into the pane spends again exactly what writing it saved. The agent has file
tools; let it read the file, when it needs it, as many times as it needs it.

Delivery goes through the [bus](orchestration.md), which means it obeys the same rules as any
other message: an agent mid-turn has it **queued** until it reaches its prompt, and an agent at
a permission prompt is never typed at. A drag that approved a file deletion because you dropped
it on the wrong row would be the worst bug this feature could have.

A service is never a valid target. Handing a note to `npm run dev` would type prose at a
process that is not reading.

## Catching the compaction

The nudge is the part only horde can do: it can see the warning on an agent's screen and the
agent cannot act on it unprompted.

```toml
[agents]
memory_nudge = true
```

Off by default, like every other thing horde does without being asked. With it on, an agent
whose screen says it is nearly out of context — under 25% left, or a warning with no number
at all — is told once:

> You are running low on context and will compact shortly, losing the detail of this
> conversation. Before that happens, save what you would be sorry to lose: run
> `horde memory save <name>-context` and pipe in what a fresh agent on this project would need
> and could not work out from the code…

The number is read rather than the phrase merely matched, because agents print this warning
early and keep printing it: "Context left until auto-compact: 45%" is not news, it is most of a
session, and nudging there would interrupt ordinary work every time.

It fires **once per fill-up**, not once per session. The flag clears once the warning has been
gone for ten minutes, so an agent that compacts, works and fills up again is told again, while
one whose status line merely flickered for a tick is not — an early version cleared the moment
the warning left the screen and earned a second nudge from a redraw thirty seconds later. Like
every nudge it goes through the bus, so it lands at the agent's next prompt rather than racing
whatever it is emitting.

The warning is matched against the detection window **joined**, not line by line. A status line
is the first thing a narrow pane wraps, and `Context left until auto-compact: 6%` arrives as
`…until auto` + `-compact: 6%` — two lines that match nothing on their own, and a percentage
detached from the phrase that introduced it.

## The sidebar section

MEMORY sits between AGENTS and SERVICES, and appears only when a project has saved something —
most sessions never do, and a rule and a label announcing that would be three rows spent on a
non-event.

Rows show the **name**, not the title: a note is addressed by its name, and a row showing prose
you cannot type would leave you hunting for the filename it stood for. The right-hand column is
how long ago it was written, which is the one thing that tells you whether it is still true.

Two behaviours worth knowing:

- **A lens never hides the notes.** A lens is a question about what your agents are doing, and a
  note is not doing anything — it is the context you reach for *while* answering that question.
  Narrowing to `needs you` and losing the note you were about to hand over would take it away
  at the only moment it was wanted.
- **A squeeze drops the notes before the servers.** Notes accumulate for the life of a project
  and servers do not, so the unbounded section yields to the bounded one; otherwise a project
  with a year of notes would eventually push a blocked dev server off the panel. Notes are
  listed newest first, so a truncated list is still the useful end of it.

## Over the socket

| Method | Params | Does |
|---|---|---|
| `memory.save` | `name`, `body`, `from` | writes `.horde/memory/<name>.md` |
| `memory.list` | `from` | this project's notes, newest first |
| `memory.show` | `name`, `from` | the body |
| `memory.give` | `name`, `to`, `from` | hands it to an agent through the bus |
| `memory.rm` | `name`, `from` | deletes it |

`name` is a single path component — letters, digits, dashes, dots and underscores. Anything
else is rejected rather than sanitised: silently rewriting what someone asked for is how you
end up with two notes that disagree about which file they are.
