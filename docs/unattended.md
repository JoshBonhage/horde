# Unattended

Everything else in horde waits to be asked. The bus routes a message someone sent, the board
holds work someone added, an agent claims a task because it was told to look. That makes horde a
workshop: capable, and inert until you are in it.

This page is the part that acts when the room is empty, and the part that tells you it did.

- **[Triggers](#triggers)** — rules that put work on the board on a schedule.
- **[Reaching you](#reaching-you)** — how the daemon gets a message to you when nothing is
  attached. Configured under `[notifications]`; see
  [configuration](configuration.md#notificationscommand).

Nothing here is on by default. Acting with nobody watching is a different promise from running
side by side, and it has to be asked for.

## Arming it

```toml
# ~/.config/horde/config.toml
[triggers]
unattended = true
```

Until that is set, rules can be added and listed but never fire. `horde trigger add` says so
rather than letting you walk away expecting something to happen.

When it is set, the sidebar footer carries the count:

```
◇ 3 tasks open
◈ 2 triggers armed
```

That row exists because "horde is allowed to act on its own" should never be something you have
to remember rather than see.

## Triggers

```bash
horde trigger add --every 30m --task "run the tests, report anything failing"
horde trigger add --at 09:00 --task "review yesterday's diff for anything broken"
horde trigger add --every 2h  --to reviewer --body "anything outstanding?"

horde trigger list           # every rule, when it last fired, what it does
horde trigger fire 1         # run one now, whatever its schedule says
horde trigger off 1          # keep it, stop it firing
horde trigger off --all      # the kill switch
horde trigger rm 1
```

**`--task` is the one to reach for.** The work lands on the board, and the nudge that already
exists finds whichever agent is free. That means a rule never has to know who is idle, and the
exclusivity guarantee stays where it already is — in the board's compare-and-set claim. Adding
agents adds throughput with no change to the rule.

`--to` pushes a line at one named agent instead. It bypasses the board, so it also bypasses
everything the board guarantees; worth it only when the work genuinely belongs to that agent.

### `horde trigger fire`

The only way to test a rule set for nine in the morning at any other time of day. It skips every
guard — the master switch, the schedule, the one-in-flight check, the hourly ceiling — because
all of those exist to bound what horde does *unasked*, and this is asked.

It is journaled exactly as an automatic firing is, and does count against the hour's ceiling, so
the record can never disagree with the guard that reads it.

### What keeps it from running away

The mechanism is a timestamp comparison. The engineering is the guards:

| Guard | Why it exists |
|---|---|
| Master switch, off by default | A fresh install never acts on its own. |
| One piece of work in flight per rule | Yesterday's task still on the board is the reason not to add today's. A skip does **not** spend the interval, so the next one goes as soon as the board clears. |
| 60s floor on `--every` | Anything faster is a busy-wait with extra steps. |
| 12 firings per rolling hour, across all rules | Agents can create triggers, so the failure mode is not one bad rule but fifty. Hitting it warns once an hour rather than silently doing nothing. |
| A failed action still counts as a firing | A rule pointing at an agent that no longer exists would otherwise retry every 150ms for as long as you are away. |

### Reading back what it did

Every firing is journaled, and the digest grows a section for them:

```
while you were away · 3h

  horde decided
    ▸ #2 put task #14 on the board: run the tests, report anything failing
    ▸ #1 put task #15 on the board: review yesterday's diff

  board
    ● #14  run the tests, report anything failing  [worker0]
           → 118 pass, 1 failing in bus.rs
```

Above the board on purpose: a firing is the reason some of that work exists. This section is the
feature, not the changelog for it — a machine that acts while you are away is only worth arming
if you can read back what it decided to do.

Triggered tasks are tagged with their origin on the board itself (`by: trigger:2`), which is both
the audit trail and how the one-in-flight guard finds them again.

### Timing details

- `--every` counts from the **last firing**, not from a fixed grid, so a slow action cannot make
  the next one due the moment it finishes.
- A new rule measures from when you added it. `--every 30m` waits thirty minutes; `--at 09:00`
  added in the afternoon waits for tomorrow rather than deciding it is nine hours late.
- `--at` is **local** time, and fires **late rather than never**: if the daemon was down at nine,
  a rule that has not run since before nine runs when it comes back. Being told at eleven that
  yesterday's diff wants reviewing beats not being told. A DST shift can put this an hour out
  twice a year, which is not worth a date library.

## Reaching you

The daemon is the only part of horde still awake while you are away, and it can now say so. Two
sinks, configured under `[notifications]`:

- `delivery = "system"` — a macOS notification, fired from the daemon.
- `command = "~/bin/horde-ping"` — your own program, with the summary as `$1` and the full
  `horde digest --json` payload on stdin. This is the whole of horde's reach: Pushover, Telegram,
  ntfy, `mail` are a two-line script you own, which is why there is no HTTP client here and
  nowhere to keep a token.

Alerts fire **only while nothing is attached** — when you are looking at the screen, the toast
already said it. The full rules, and what earns an alert, are in
[configuration](configuration.md#notificationscommand).

## Agents can do this too

`horde trigger add` works from inside a pane, so an agent can schedule its own follow-up. The
rule records who added it, and `horde trigger list` shows it.

One thing is refused: a trigger's action cannot create another trigger. Agents creating rules is
the interesting part; rules creating rules is a closed loop with no human in it.

## What this deliberately does not do

**Spawn agents.** A rule can only put work where an existing agent will find it. Starting an
agent with nobody present is a larger change in blast radius than scheduling one, and it wants
its own cap and its own provenance tracking first.

**Reach inward.** There is no way to reply to a notification and have it come back into horde.
That would turn a local unix-socket tool into an authenticated network service. The cheap honest
version needs no horde code: make your notify script a two-way bot that shells out to
`horde task add`, and the trust boundary stays in code you wrote.
