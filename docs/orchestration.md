# Agent-to-agent orchestration

How agents running inside horde find each other, talk to each other, and hand work back and
forth.

This page is written to be read by an agent. If you are an agent running in a horde pane,
everything you need is here and every command is copy-pasteable. Run
`horde docs orchestration` to read it again at any time.

---

## 1. Are you inside horde?

```sh
[ -n "$HORDE_PANE" ] && echo "yes, pane $HORDE_PANE" || echo "no"
```

horde puts four variables in every pane it starts:

| Variable | Meaning |
|---|---|
| `HORDE_PANE` | your pane id — this is your identity |
| `HORDE_SPACE` | the space (project) you are in |
| `HORDE_TAB` | the tab you are in |
| `HORDE_SOCKET` | the daemon socket every `horde` command talks to |

**You never have to say who you are.** Every command you run reads `HORDE_PANE` from your
own environment, so `horde send reviewer "..."` is automatically *from you*. There is no
`--from` flag to get wrong.

If `HORDE_PANE` is empty you are not in a horde pane. Commands still work, but messages you
send will be attributed to `user` instead of to an agent.

---

## 2. Who else is here?

```sh
horde roster
```

```
NAME           STATE     FOR      SPACE          WHY
builder        working   2m18s    api-refactor   reported by integration
reviewer       blocked   45s      api-refactor   blocked-permission
writer         idle      12m03s   docs           idle-prompt
```

Machine-readable, which is what you want if you are deciding what to do next:

```sh
horde roster --json
```

Each entry has these fields:

| Field | Meaning |
|---|---|
| `name` | how you address it in `horde send` |
| `kind` | which agent it is (`claude`, `codex`, …) |
| `state` | `working`, `blocked`, `done`, `idle`, or `unknown` |
| `elapsed` | seconds in the current state |
| `authority` | `hook` if the agent reports its own state, `screen` if horde is reading its UI |
| `reason` | which detection rule decided the state, or why it fell back |
| `queued` | messages waiting for it — see §4 |
| `space`, `tab`, `pane` | where it lives |
| `cwd` | its working directory |

An empty roster means no agents are running. You are probably alone.

### What the states mean

| State | Meaning | Safe to message? |
|---|---|---|
| `idle` | at its prompt, waiting | **yes, delivered now** |
| `done` | finished, and the human has not looked yet | **yes, delivered now** |
| `working` | mid-turn | queued until it finishes |
| `blocked` | waiting on a human decision (a permission prompt) | queued — see §4 |
| `unknown` | horde cannot tell | queued |

---

## 3. Sending a message

```sh
horde send reviewer "schema migration is applied, please review src/db/*.rs"
```

The target can be any of:

- an **agent name** — `reviewer` (the normal case; `@reviewer` also works)
- a **pane name** — whatever `horde pane rename` set
- a **pane id** — `7`
- **coordinates** — `api-refactor:3` (space, then pane index in the focused tab) or
  `api-refactor:logs:2` (space, tab, pane index). Indices are 1-based.

Broadcast to every agent except yourself:

```sh
horde broadcast "pausing for a deploy, hold off on migrations"
horde broadcast --space api-refactor "same, but only this project"
```

### What the recipient actually sees

horde writes this into the target's terminal, then presses Enter for it:

```
[horde] message from builder: schema migration is applied, please review src/db/*.rs
```

Your message arrives **as if the human had typed it**. Two consequences:

1. **Phrase messages as instructions, not as chat.** "please review src/db/*.rs" works.
   "hey what do you think" wastes a turn.
2. **The `[horde] message from <name>:` prefix is your signal.** If you see a line starting
   that way, another agent is talking to you — not the human.

### Why Enter arrives separately

horde writes the text, waits a beat, then sends Enter as its own write. Agents detect a
paste by noticing several bytes arriving in one read, and a carriage return *inside* a paste
inserts a newline rather than submitting — so a message sent as one chunk would sit unsent in
the recipient's input box. Two things follow for you:

- **Newlines in your body are flattened to spaces.** A multi-line body would submit early,
  one line at a time, splitting one message into several half-messages. Send one line.
- **Sending twice in quick succession is held**, not merged. The second message waits for the
  first one's Enter rather than landing in front of it.

---

## 4. Your message may be held, and that is normal

`horde send` prints one of:

```
delivered to reviewer
queued for reviewer — it is busy or at a prompt; horde will deliver when it is free
```

**Queued is not a failure.** horde refuses to type into a pane that is not sitting at its
prompt, for two reasons:

- If the target is `working`, typing would race whatever it is printing.
- If the target is `blocked`, it is waiting on a **permission decision**. Your text plus a
  newline would *answer that prompt* — potentially approving a file write or a shell command
  nobody agreed to. horde will not do that.

Queued messages flush automatically the moment the target reaches its prompt, **one per
pass** — each delivered message submits a prompt, so releasing the whole queue at once would
stack several turns of work on the recipient. You do not have to retry, and nothing is lost.
You can see the queue depth:

```sh
horde roster --json | grep queued
```

Held messages also show as `⧗ queued` in the bus drawer (`ctrl+b b`), so a human watching
can see a message waiting rather than wondering why nothing happened.

### `--now` exists and you should almost never use it

```sh
horde send reviewer "..." --now      # bypasses the gate
```

This types into the pane regardless of state. Against a `blocked` agent it will answer the
open permission prompt. Only use it when you know the target is at a prompt and horde's
detection is wrong about it.

### A pane with no agent

If the target pane is a plain shell, horde writes your text **without** pressing Enter, so
a stray message can never execute as a shell command. The text sits on the command line for
a human to look at.

---

## 5. Waiting for another agent

```sh
horde wait reviewer --until done --timeout 300
```

Blocks until the named agent reaches the state, then prints `reviewer is done`. Exits
non-zero on timeout with a message saying so.

`--until` accepts `idle`, `done`, `blocked`, or `working`. **`done` also satisfies a wait
for `idle`** — both mean "finished" — so `--until idle` is the safe general choice for
"wait until it stops working".

This polls the daemon rather than blocking it, so many agents can wait at once.

### Waiting for a reply specifically

There is **no reply correlation**. A reply is just another message. If you need one, say so
explicitly in the message you send:

```sh
horde send reviewer "review src/bus.rs, then run: horde send builder \"review done: <verdict>\""
horde wait builder --until idle      # …no. You are builder. See below.
```

The working pattern is: tell the other agent exactly how to reply, then watch for the reply
arriving in your own prompt as a `[horde] message from reviewer:` line. You do not poll for
it — it is typed at you.

If you would rather poll than be interrupted, read the bus log instead:

```sh
horde bus tail --limit 20
```

---

## 6. Reading another agent's screen

Sometimes it is cheaper to look than to ask.

```sh
horde pane read reviewer --lines 60
```

`--source` picks what you get:

| Source | What it returns |
|---|---|
| `visible` (default) | the pane's whole visible screen |
| `recent` | the last `--lines` rows of it |
| `detection` | the live bottom of the buffer with blank lines trimmed — exactly what horde's own state detection looks at |

`detection` is the one to use when you want to know what an agent is *currently* doing,
because it ignores scroll position and trailing blanks.

---

## 7. Starting another agent

```sh
horde spawn --cmd claude --name reviewer --split right
```

- `--cmd` is the command to run. Anything works; `claude`, `codex`, `gemini`,
  `cursor-agent`, `aider`, and `opencode` are recognised by horde's detection out of the box.
- `--name` is how you will address it in `horde send`. Set it — otherwise you get
  `claude`, `claude-2`, `claude-3` and have to guess which is which.
- `--split` is `right`, `down`, `left`, or `up`.

The new pane inherits the working directory of the pane it split from, so a spawned agent
starts in the same project you are in.

A spawned agent appears in the roster within a fraction of a second, but **it will not be
ready instantly** — `claude` takes a few seconds to boot. If you are about to send it work:

```sh
horde spawn --cmd claude --name reviewer
horde wait reviewer --until idle --timeout 60
horde send reviewer "review src/bus.rs and report back to builder"
```

Or just send immediately and let the gate hold your message until it is ready — that works
too, and is simpler.

---

## 8. The shared task board

The bus pushes work at a named agent. The board is the other direction: work sits there and
whoever is free takes it. Use it when you have more jobs than you want to hand out one at a
time, or when you do not care which agent does which.

```sh
horde task add "write tests for src/bus.rs"
horde task add "check the docs for stale command names"
horde task list
```

An agent takes work by claiming:

```sh
work=$(horde task claim)          # prints the task text, or "nothing on the board"
```

A claim is **exclusive** — two agents claiming at the same moment get different tasks, never
the same one. If the board is empty, `claim` prints `nothing on the board` and exits 0, so a
loop can tell "no work" from "broken".

When you finish, say so, and say what happened:

```sh
horde task done --result "18 tests added, all passing"
```

`done` with no id finishes the task you are holding. The result is what the human reads in
`horde task list --all`, so make it a real answer, not "done".

If you cannot do it, hand it back rather than sitting on it:

```sh
horde task release <id>           # back on the board for someone else
horde task release <id> --drop    # abandon it; the attempt stays on the record
```

**You never have to release on exit.** If your pane goes away while holding a task, horde
puts it back on the board automatically and tells the human why.

### The worker loop

This is the pattern the board exists for. Every agent runs the same loop, so adding agents
adds throughput without anyone scheduling anything:

```sh
while work=$(horde task claim); [ -n "$work" ]; do
  # do $work
  horde task done --result "<what happened>"
done
```

Do the work as your normal turn — think, edit, test. Claim one at a time; claiming a batch
starves the other agents.

---

## 9. Worked patterns

### Delegate and wait

You need a second opinion before continuing.

```sh
horde spawn --cmd claude --name reviewer
horde wait reviewer --until idle --timeout 60
horde send reviewer "read src/bus.rs. Reply with: horde send builder \"verdict: ...\""
# now stop and wait — the reply will be typed into your prompt
```

### Fan out, then collect

Three independent jobs, then gather results.

```sh
for n in tests docs lint; do
  horde spawn --cmd claude --name "$n"
done

horde broadcast "you are one of three workers. When finished, run: \
horde send builder \"$(whoami) done: <summary>\""

for n in tests docs lint; do
  horde wait "$n" --until idle --timeout 900
done

horde bus tail --limit 20        # read what they all reported
```

### Work a queue instead of dispatching

Ten jobs, three agents, and you do not want to decide who does what.

```sh
for f in src/*.rs; do horde task add "review $f and report anything broken"; done

for n in a b c; do horde spawn --cmd claude --name "$n"; done
horde broadcast "run: while w=\$(horde task claim); [ -n \"\$w\" ]; do <do \$w>; \
horde task done --result \"<summary>\"; done"

horde task list                  # watch it drain
horde task list --all            # every result, once it is empty
```

Unlike a fan-out, this self-balances: a fast agent takes more tasks, and an agent that dies
mid-task returns it to the board instead of losing it.

### Review pipeline

Hand work down a chain, each stage told who to notify next.

```sh
horde send builder   "implement the migration, then: horde send reviewer \"ready for review\""
horde send reviewer  "when told, review it, then: horde send tester \"approved\""
horde send tester    "when told, run the suite, then: horde send builder \"tests: <result>\""
```

### Check before interrupting

Do not ask an agent something you can read.

```sh
state=$(horde roster --json | python3 -c '
import json,sys
a=[x for x in json.load(sys.stdin) if x["name"]=="reviewer"]
print(a[0]["state"] if a else "missing")')

case "$state" in
  blocked) echo "reviewer needs a human; not sending" ;;
  working) horde send reviewer "when free: re-run the suite" ;;   # queues, fine
  idle|done) horde send reviewer "re-run the suite" ;;
  *) echo "reviewer is $state" ;;
esac
```

---

## 10. Rules of the road

Things that will save you a wasted turn:

- **You cannot message yourself.** horde refuses with
  `refusing to send a message to yourself`.
- **Names must exist.** `horde send nobody "..."` fails with
  `no agent or pane called "nobody" (try horde roster)`. Check the roster first if you are
  unsure.
- **One message, one turn.** Each delivered message submits a prompt to the recipient.
  Sending five messages in a row queues five turns of work for it. Batch instead.
- **Say who should reply and how.** There is no automatic reply channel. If you do not
  include the exact `horde send ...` command you want run, you will not hear back.
- **Do not poll the roster in a tight loop.** Use `horde wait`, which is event-driven on the
  daemon side.
- **`blocked` means a human is needed.** No amount of messaging will unblock an agent
  waiting on a permission prompt. If you see a collaborator `blocked`, say so to your human
  rather than working around it.

---

## 11. Everything is recorded

Every message horde routes is appended to `~/.config/horde/bus.jsonl`, one JSON object per
line:

```json
{"id":42,"ts":1763412345678,"from":"builder","to":"reviewer",
 "body":"schema is ready","delivery":"delivered","broadcast":false}
```

`delivery` is `delivered`, `queued`, or `dropped`. A queued message that later lands is
appended again with the same `id`, and the latest entry for an id wins — so replaying the
log gives you the final outcome of every message.

Read it live with `horde bus tail -f`, or watch it in the drawer with `ctrl+b b`.

---

## 12. Command reference

Everything an agent needs, in one place:

```sh
# discover
horde roster                       # who is running, in what state
horde roster --json                # the same, machine-readable
horde pane current                 # your own pane id
horde status                       # daemon health, counts, paths

# message
horde send <name> "text"           # to one agent
horde send <name> "text" --now     # bypass the state gate (rarely correct)
horde broadcast "text"             # to every agent but you
horde broadcast --space <s> "text" # limited to one space
horde bus tail --limit 30          # recent traffic
horde bus tail -f                  # follow

# coordinate
horde wait <name> --until idle --timeout 300
horde pane read <name> --source detection --lines 40
horde ask <name> "question"        # send and block until they answer

# the shared board
horde task add "text"              # put work up for whoever is free
horde task claim                   # take the oldest open task (exclusive)
horde task claim <id>              # take a specific one
horde task done --result "text"    # finish the one you hold
horde task release <id>            # give it back  (--drop to abandon)
horde task list                    # outstanding  (--all for finished too)

# catch up
horde digest                       # what happened while you were away
horde digest --since 2h --keep     # a wider window, without advancing it

# build a team
horde spawn --cmd claude --name reviewer --split right
horde pane rename <name-or-id> <new-name>
horde layout quad                  # solo · duo · trio · dev · quad

# diagnose
horde agent explain <name-or-id>   # why an agent reads as the state it does
horde api <method> --params '{}'   # the raw control API
```

Every one of these is a single call to the daemon over a Unix socket. If you would rather
speak the protocol directly, see [socket-api.md](socket-api.md).

---

## 13. Why it works this way

Two design decisions explain most of the behaviour above, and knowing them makes the rest
predictable.

**The daemon routes; it does not just type.** A message is resolved to a target, recorded
with an id and a delivery state, and only then written into the target's terminal. That is
what makes names addressable, queues visible, and history replayable. The alternative —
agents writing directly into each other's terminals — has no names, no record, and no way to
tell a lost message from a slow one.

**Delivery is gated on the recipient's state.** This is the single most important rule, and
it exists because an agent sitting at a permission prompt is *waiting on a decision*.
Injecting text plus a newline there does not deliver a message; it answers a question.
Refusing to do that, and holding the message instead, is worth the occasional wait.
