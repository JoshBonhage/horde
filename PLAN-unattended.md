# Unattended horde

**Today horde is a workshop: capable, and inert until someone is in it. This is the plan to
give it initiative (it acts when nobody is watching) and reach (it can tell you, wherever you
are).**

Those are two different promises, and the second one has to land first. See
[Why the notifier comes first](#why-the-notifier-comes-first).

---

## What already exists

Three things mean this is less new subsystem than it looks.

**One trigger is already written.** `Engine::nudge_for_tasks` (`src/daemon/mod.rs:154`) watches
state and pushes an agent unprompted, and it already carries the guard logic a scheduler needs:
dedupe keyed on the agent's idle period, a "never wake more agents than there is work for"
budget, and a config kill switch. It is a scheduler with one hardcoded rule. Most of this plan
is generalising what that function already knows.

**Every action is already an engine operation.** `task.add`, `bus.send`, and `agent.spawn`
(`src/daemon/rpc.rs:351`) are synchronous calls on the single-threaded engine. A trigger engine
needs no new capabilities — only new callers.

**Reach is in the wrong process.** `notify_system` (`src/client/mod.rs:220`) is called from
`Client::toast`, so notifications exist only while a TUI is attached — precisely when you don't
need them. The daemon, the part that survives detaching, has no outbound path at all.

## The decisions this is built on

| | |
|---|---|
| **Spawning** | In scope, behind a cap. Triggers may start agents, not just wake them. |
| **Declaration** | CLI writing to an append-log, like the board. Agents can create triggers. |
| **Permissions** | Full. Unattended agents run with the freedom you'd give one by hand. |
| **Sinks** | `command` hook and `system` (osascript). No built-in HTTP client, no secret store. |

The first three together are the most autonomous configuration available, which is a deliberate
choice and changes what the build has to get right — see
[What the autonomy costs](#what-the-autonomy-costs).

---

## The model

A trigger is **when × what × guard**. The guard is the whole engineering problem; when and what
are plumbing over primitives that exist.

```
when                          what                      guard
────────────────────────      ──────────────────        ──────────────────────────
every <dur>                   task add "<text>"         armed?
at <cron>                     send <agent> "<text>"     already fired for this?
board empty                   notify "<text>"           rate: per-trigger, global
agent stuck <dur>             spawn <cmd> --name <n>    spawn cap
path <glob> changed                                     quiet hours, attach scope
command exits nonzero                                   depth: no trigger begets a trigger
```

**`task add` is the default action and the one to reach for.** It puts work on the board and
lets the *existing* nudge do the waking, which means the scheduler never needs to know who is
free, and exclusivity stays where it already is — in `Board::claim`'s compare-and-set. A trigger
that adds a task composes with everything already built. A trigger that sends or spawns is
bypassing a mechanism that works.

---

## Phase 1 — the notifier — **shipped**

`src/daemon/notify.rs`, called from `tick`. Two departures from the plan as written, both found
by reading the code it was going to change:

- **The client keeps its own system notification.** The plan said move it out of `Client::toast`.
  But that path isn't wrong, only incomplete — horde attached in another window with
  `delivery = "system"` is a legitimate setup. So the daemon's alerts fire *only while detached*,
  the client keeps the attached case, and the two never overlap. Crisper rule, nothing broken.
- **No exit-status alerts yet.** The plan wanted "a pane exiting nonzero", but `journal::Entry`
  keeps no status — `Event::PaneExited` carries one and the journal drops it. Adding a field
  there is small and separate; reading the same-tick event instead would make the alert depend on
  the coalesce window not having just closed. Deferred rather than bodged.

What fires an alert is therefore the digest's own top facts: an agent that has wanted a human or
sat on an unread finished turn for a full minute, and the board emptying. Four rules keep it
readable — detached only, settled facts only, one ping per wait (keyed on state entry, like
`nudged_since`), one ping per five minutes carrying the whole window as `Digest::headline()`.

One correctness detail worth keeping: the reason window is *exclusive* of `since` where the
digest's is inclusive. A task closing in the same millisecond as an alert is already in that
alert's payload, and counting it again is how one finished board sends two notifications. A test
landed on that millisecond.

- `notify::consider(eng, Reason)` called from `tick`, on a deliberately small set of reasons: an
  agent `Blocked` longer than N, the board going empty, a trigger firing, a pane exiting
  nonzero, a trigger's spawn hitting the cap.
- **Coalescing, not per-event pings.** One notification per window carrying the digest
  one-liner — *"1 agent needs you, 2 tasks done"* — because a ping per event trains you to
  ignore pings. The one-liner already exists for the reattach toast.
- Sinks:
  - `system` — osascript, as today but fired from the daemon. **Verified**: the daemon is
    `setsid`'d but stays in the `Aqua` bootstrap namespace it inherited, so osascript exits 0 and
    the notification appears. No `launchctl asuser` wrapper needed. It would fail if horde were
    first started over SSH with no GUI session, where `command` is the sink that still works.
  - `command` — spawn a user program with `digest --json` on stdin. This is the entire answer to
    reach: Pushover, Telegram, ntfy, Slack, email are all a script you own, and horde grows
    neither an HTTP client nor a place to keep a token.
- **The engine must not block.** Today's `notify_system` uses `.spawn()`, not `.output()` —
  carry that over, and reap the children so a notify-heavy hour doesn't accumulate zombies. A
  sink that hangs must cost nothing; a slow user script cannot become a stalled multiplexer.
- `journal::Kind::Notified`, so "did it actually tell me" is answerable after the fact.

Useful on its own with zero triggers: detach, get pinged when an agent blocks. That standalone
value is why it's first.

## Phase 2 — the trigger engine — **shipped**

`src/daemon/triggers.rs`, fired from `tick` just before the notifier so a firing is something the
same pass can already report. Sources: `every <dur>` and `at HH:MM` (local, via `libc::localtime_r`
— no date crate). Actions: `task add` and `send`. All six guards in place, `horde trigger
add/list/rm/on/off/fire`, a "horde decided" digest section above the board, and `◈ n triggers
armed` in the sidebar footer.

Three decisions that departed from the plan as written:

- **Ids, not names.** The plan wanted `name`. The board already addresses work by id
  (`horde task claim 4`), `trigger list` prints the schedule and action on every row, and a name
  needs uniqueness validation to earn its keep. Dropped.
- **`fire` skips every guard.** Including the master switch and the one-in-flight check. All of
  them bound what horde does *unasked*, and a manual fire is asked. It is journaled identically
  and does count toward the hourly ceiling, so the record cannot disagree with the guard reading
  it.
- **The depth guard came free.** With actions limited to `task`/`send`, a trigger's action cannot
  create a trigger, so nothing had to be written to refuse it. It becomes real work in phase 3,
  when a spawned agent could call `trigger.add` itself.

The one-in-flight guard turned out to be the load-bearing one, and its subtlety is that a skip
must **not** spend the interval — otherwise clearing the board starts a fresh wait instead of
letting the held firing go immediately. Both halves are pinned by tests.

Verified against a real daemon: no firing while disarmed; manual fire working while disarmed;
the in-flight guard holding across ~53 ticks; automatic firing at exactly the 60s mark; the digest
section; the kill switch; and `off` surviving a restart — a restart that silently re-armed
disabled rules would be the worst possible surprise in this feature.

## Phase 2 as planned

**Store.** `src/daemon/triggers.rs`, modelled directly on `tasks.rs`: an append-log replayed
with later-entries-supersede-by-id, `CAP`-bounded, carried across rotation. The board already
solved persistence, bounding, and rotation-without-losing-live-state — copy it rather than
inventing a second pattern.

```rust
struct Trigger {
    id: u64, name: String,
    when: When, what: What,
    enabled: bool,
    created: u64, by: String,          // "user", or the agent that added it
    last_fired: Option<u64>, fire_count: u64,
    depth: u8,                          // 0 = created by a human or a plain agent
}
```

**Evaluation** hangs off the existing `tick` (`src/daemon/mod.rs:776`) behind its own interval —
a `TRIGGER_INTERVAL` of ~1s, alongside `DETECT_INTERVAL`. Cron needs second granularity; nothing
here needs to run at the attached 16ms cadence.

**Surface.** `horde trigger add --every 30m --task "..."` / `list` / `rm` / `on` / `off` /
`fire <id>` / `log`, over `trigger.*` RPC methods. `fire` is not a convenience — it's the only
way to test a 9am rule at 3pm, and without it every iteration costs a day.

**Guards**, in the order they'd save you:

1. **`unattended = false` by default.** Nothing fires until armed. A fresh install does not
   quietly start acting.
2. **Per-trigger dedupe** — don't re-fire while the last firing's task is still open or claimed.
   The generalisation of `nudged_since`.
3. **Rate ceilings** — a minimum interval per trigger, and a global firings-per-hour cap. The
   global one exists because agents can create triggers, so the failure mode isn't one bad rule,
   it's fifty.
4. **Depth: a trigger firing cannot create a trigger.** Agents creating triggers is the
   interesting part; triggers creating triggers is a closed loop with no human in it. Refuse
   `trigger.add` when the calling pane's `depth > 0`.
5. **Quiet hours and attach scope** — some rules only make sense while you're away, some only
   while you're there.
6. **`horde trigger off --all`** as one keystroke, reporting what was in flight.

**Journal and digest.** `journal::Kind::Fired`. The digest builder (`src/daemon/digest.rs:17`)
already reads the journal from a marker, so a "horde decided" section is additive: what fired,
what it produced, what it was refused by. This section is the feature, not the changelog for it.

**Sidebar.** The footer already carries `◇ 3 tasks open`. Armed unattended mode needs a marker
there too — that the machine is *allowed to act on its own* should never be something you have
to remember rather than see.

## Phase 3 — spawning — **shipped**

`What::Spawn { cmd, name }`, `triggers.max_spawned` (default 2, clamped 0–16), `spawned_by` on the
pane, and the depth guard in `rpc.rs` where the calling pane is known.

Also shipped, out of order because it was twenty lines and cron users expect it: `--days` on
`--at`, taking lists, ranges and wrapping ranges (`mon-fri`, `sat,sun`, `fri-mon`) as a bitmask
matching `tm_wday`, defaulted so rules written before the field existed replay unchanged. The day
filter beats lateness — a weekday rule missed on Friday does not fire on Saturday, because running
late is a courtesy and running on an excluded day is disobeying the rule.

**Provenance turned out to be the whole job.** `spawned_by` has to survive three separate paths or
both guards that read it quietly stop working:

- `persist.rs` — `restore = true` would otherwise bring a machine-started agent back as one horde
  thinks you started, freeing a cap slot *and* re-granting it the right to create triggers.
- `handoff.rs` — `horde upgrade` would launder it the same way, mid-session.
- `PaneInfo` — so `horde roster` can say `[by trigger #3]`.

Provenance you can launder is not provenance, and two of those three paths are ones you'd only
notice were wrong weeks later.

The cap counts *live* spawned panes, so a finished agent frees its slot. Hitting it is a warning
that still spends the interval — the alternative retries every 150ms and warns every time.

Verified end-to-end: rules parse and read back (`at 09:00 mon–fri · board: …`), a fired spawn
carries `spawned_by`, the cap refuses loudly at 1, the depth guard refuses `trigger.add` from the
spawned pane by name, and provenance survived a full daemon restart.

One bug the end-to-end run caught that no test would have: a Rust line-continuation backslash
eaten by the shell heredoc that generated the edit, leaving twenty spaces inside an error message.
Tests assert on `contains`, so they were all perfectly happy. Reading the actual output is not
optional.

## Phase 3 as planned

- `What::Spawn { cmd, name }`, with `max_triggered_agents` as a hard global cap.
- **Pane provenance.** `spawned_by: Option<TriggerId>` on the pane, which the cap counts and the
  depth guard reads. Without it there's no way to distinguish the fleet you built from the fleet
  the machine built, and both the cap and the audit trail depend on that distinction.
- Hitting the cap is a notification, not a silent no-op. A trigger that quietly stopped working
  is worse than one that failed loudly.

## Phase 4 — reactive sources — **shipped, as a condition rather than sources**

The plan wanted three new `When` variants: `path <glob> changed`, `command exits nonzero`, and
`agent stuck <dur>`. What shipped is one field, `Trigger.only_if`, and it covers more:

```bash
--every 30m --when "! cargo test -q"                       # tests are broken
--at 09:00  --when "! git diff --quiet"                    # uncommitted work
--every 15m --when "find src -newer target/debug/horde -name '*.rs' | grep -q ."   # files changed
```

A gate on the schedule rather than a source of its own, because "has anything changed" *is* a
command. That composes with both `every` and `at` instead of needing a variant per question — and
it means no file-watching dependency in a tool that has deliberately few.

`agent stuck` was dropped rather than deferred. The notifier already tells a human about a blocked
agent, and a rule that responds to "this needs a human" by doing something automated is usually
the wrong answer.

Three things the implementation had to get right:

- **Off-thread.** The engine drives every pane; waiting for `cargo test` inline freezes every
  agent's output. So a condition costs a tick or two to decide, and `Condition::Waiting` spends
  nothing.
- **An unmet condition spends the interval.** Not a firing — `fire_count` stays put, nothing is
  journaled — but the interval is consumed. This is the one that matters: without it the probe
  re-runs every tick, which for a real command is a fork bomb with good intentions. Verified live:
  one probe in 70 seconds, not the ~460 a per-tick bug would have produced.
- **`last_eval` falls back to `last_fired`.** A new field defaulting to `None` would have made
  every existing rule come due at once on upgrade.

The end-to-end run also caught a documentation bug in my own example: `!` inverts *any* non-zero
exit, so `! git -C /bad/path diff --quiet` succeeds because git failed, not because anything
changed. A negated condition fires on "could not run" as readily as on "found something". Now
documented, with the explicit form for when that distinction matters.

## Phase 4 as planned

`path <glob> changed` (debounced), `command exits nonzero` (periodic probe, e.g. `cargo test`),
`agent stuck <dur>`. Deferred because they're the sources most likely to fire in bursts, and
bursts are what the phase 2 rate ceilings need to have already proven they can absorb.

---

## Why the notifier comes first

You want to *see* what unattended horde does before you let it act. Observability precedes
autonomy — and if the order were reversed, the first week of triggers would be debugged by
reading log files after the fact, which is exactly the experience that gets a feature switched
off and left off.

## What the autonomy costs

Full permissions plus scheduled spawning plus agent-authored triggers means an agent can run
tool calls at 3am with nobody to approve them. That is a real change in blast radius, and it's
the choice made here deliberately — it's a personal tool on one machine.

What follows from it: the journal is not bookkeeping, it's the mechanism. Every firing, every
spawn, every refusal, and every notification recorded, because after-the-fact review is the only
review there is. Phase 1's `Kind::Notified` and phase 2's `Kind::Fired` are carrying more weight
than their size suggests.

## Deliberately out of scope

**Inbound reach** — replying from your phone. It turns a local unix-socket tool into an
authenticated network service, which is a much larger promise than anything above. The cheap
honest version needs no horde code: your notify script is a two-way bot that shells out to
`horde task add`, and the trust boundary stays in code you wrote.

**A built-in webhook.** The `command` sink covers it, and a URL in config means a token in
config.

---

## Worth doing before any of this

A `launchd` plist that runs `horde task add "review yesterday's diff"` at 9am. Zero horde code —
and combined with the nudge that already exists, it delivers unattended work today. Run it for a
few days first: it answers "is unattended agent work actually useful to me" before a week goes
into a scheduler, and whatever goes wrong with it is a phase 2 guard you'd otherwise have
discovered the hard way.

What it can't do, and therefore what justifies the rest: spawn a worker when none is running,
react to anything, or leave any record of why something happened.
