# Agents: detection and state

horde works out what each agent is doing so the sidebar can tell you which ones need you.

## The six states

| State | Glyph | Meaning |
|---|---|---|
| `working` | `◐` (animated) | actively running a turn |
| `blocked` | `◍` | waiting on a human decision — an approval or permission prompt |
| `done` | `●` | finished, and you have not looked yet |
| `idle` | `○` | at its prompt, and you have seen it |
| `unknown` | `◌` | horde cannot tell confidently |
| `serving` | `◆` | a dev server or watcher, up and holding — see [Services](#services) |

`blocked` is deliberately strict. It is set only when a visible approval, question, or
permission UI is matched. **Silence is never treated as blocked** — an agent that has gone
quiet is not the same as one asking you something, and conflating them would make the
attention count useless.

`done` is derived, not reported. A `working → idle` transition while you are not looking at
the pane becomes `done`, and focusing or typing in the pane clears it back to `idle`. That is
the "finished while you were away" signal.

## Services

Not everything in a pane is an agent. A manifest declaring `class = "service"` describes
something that *runs* rather than something you talk to: `npm run dev`, `vite`, a file
watcher, a tunnel. The bundled `dev` manifest covers the common ones.

A service uses two states and no others — `serving` when it is up, `blocked` when it needs
you (the port is taken, the build is broken) — and horde treats it differently in the places
where agent behaviour would be nonsense:

- it is **counted separately**, so three panes of `npm run dev` cannot make a quiet session
  read as a busy one, and it does not appear in a project's agent count
- it **never becomes `done`**: `done` means "you have not read this yet", and nobody is going
  to read a page-load log
- it is **never handed board work** and never receives a bus message

`working` is deliberately not one of its states. A compile that finishes in 300ms cannot be
seen by a detector that looks every 640ms, so a "compiling" state would be a flicker you
cannot read rather than a signal you can.

The colour is its own — `theme.custom.serving` — for the same reason: a dev server is
background texture you want to be able to *not* look at, which it cannot be while it shares a
colour with an agent mid-turn.

### A shell prompt is neither

If the pane's foreground process is a shell, horde detects nothing there, whatever the screen
still shows. Scrollback is not evidence: a terminal that has merely *mentioned* an agent — a
`claude --help`, a PR body with a "Generated with Claude Code" footer, a `git log` — used to
keep matching that agent's `detect` patterns and sit in the roster as an agent that was not
running.

## Answering them all in one place

`ctrl+b A` opens the approval queue: every agent that is `blocked`, longest wait first, with
the question read off its screen and answerable from there.

```
◍ reviewer   Halo Suite   waiting 12m
  Do you want to make this edit to src/mux.rs?
    1  Yes
    2  Yes, and don't ask again
    3  No, and tell Claude what to do differently
```

Detection already knows *that* an agent is waiting. This works out *what it asked*, which is
the difference between a sidebar saying six agents need you and one place that shows you the
six questions.

The parse is a heuristic, and deliberately generic rather than per-agent: every agent draws a
question the same way, as a line ending in `?` above a numbered list, because that is what a
terminal makes easy. One parser covers Claude, Codex, Gemini and Cursor without six manifests
having to agree on a region. It handles a plain `(y/n)` prompt too, and a prompt box wrapped
by a narrow pane.

It will not guess. Nothing matched means no question, and the queue lists the agent with
"open the pane" rather than inventing a prompt. A wrong parse here costs a question you go and
read in its own pane; that is why it is allowed to be a heuristic at all, where a wrong
*state* would be a lie the whole UI repeats.

See [keys](keys.md#the-approval-queue) for what each key does and why the queue is as narrow
as it is.

## Two tiers, one authority per pane

### Tier 1: lifecycle hooks (authoritative)

```sh
horde integration install claude
```

The agent reports its own state. While hook reports are fresh, they win outright and the
screen manifest is **not consulted** — one authority per pane, never two.

For Claude Code, horde maps hooks like this:

| Hook | State |
|---|---|
| `UserPromptSubmit`, `PreToolUse` | `working` |
| `Notification` | `blocked` |
| `Stop` | `idle` (→ `done` if you are not looking) |
| `SessionStart` | records the session id, for resume after a restart |

Two events are deliberately ignored: anything carrying an `agent_id` (a subagent's lifecycle
says nothing about whether the pane needs you) and `SubagentStop` (it can arrive *after* the
main turn already stopped, so treating it as activity would revive an idle pane).

Reports go stale after 90 seconds. If an integration is installed but the agent stops
reporting — killed mid-run, crashed — horde falls back to the screen manifest rather than
showing a stale state forever.

Installing is merge-safe: it backs up `~/.claude/settings.json` first, leaves every other
tool's hooks alone, and is safe to re-run.

### Tier 2: screen manifests (fallback)

Without hooks, horde identifies the foreground process and matches rules against the pane.

**Which agent is in a pane** is decided in order of how much the signal can be trusted:

1. a fresh **hook report** — the agent said so itself
2. an agent's **foreground process name** from `ps` — a definite answer
3. the **command the pane was started with** — what we were asked to run
4. an agent's **screen patterns** — a guess, and the only ambiguous signal
5. a **service's** process name, then its screen patterns

Services come last, after even an agent's screen guess, because a service manifest names
launchers rather than programs: `npm` and `bun` run whatever you ask them to, including an
agent. "A service is what a pane is when it is not an agent" costs nothing, and means a broad
process list can never quietly relabel something you were talking to.

`ps` reports what a process calls itself, and node tooling rewrites its own title — a Next dev
server shows up as `npm run dev` — so matching is against the first word of it.

The order matters because agent UIs share phrases. `esc to interrupt` appears in Claude Code,
Codex, and Cursor Agent alike, so a `detect` list containing it makes several manifests claim
the same pane — and if the winner is decided by hash order, the pane flickers between names
from one scan to the next. Every bundled `detect` list is specific to its own agent, and the
iteration is sorted.

#### Regions: what a rule looks at

A rule matches a **region**, not the whole snapshot. This is the single most important idea
in the file, because both ways screen detection goes wrong are regional:

| Region | What it is |
|---|---|
| `osc_title` | the terminal title, set by escape sequence |
| `whole_recent` | the whole snapshot (the default) |
| `bottom_non_empty_lines(N)` | the last N non-blank lines — the live status area |
| `after_last_horizontal_rule` | everything below the last rule — usually the current dialog |
| `prompt_box_body` | between the last two rules — the composer itself |

**`osc_title` is the one that changes the game.** The title is an escape sequence, not
characters in the grid, so it is never truncated by a narrow pane and never left over from
scrollback. Claude Code sets it to a spinner glyph while a turn runs and to `✳` at rest — so
horde can read the state of a 38-column pane, where the on-screen marker is not rendered at
all.

#### Priority, and why the idle title ranks low

Rules carry an explicit `priority`; highest wins, and declaration order only breaks ties.
Order-dependence is how a manifest rots — inserting a rule silently changes what the ones
below it mean.

The priorities encode an asymmetry worth understanding:

```
osc_title_working    1100   ← nothing else can be true while a turn runs
transcript_viewer    1000   ← suppression (see below)
permission_prompt     900
status_line_working   600   ← screen fallback, for when the title is unavailable
composer_idle         500
osc_title_idle        250   ← weak: only means "not generating"
```

The **working** title is authoritative. The **idle** title is not: it only says the agent
is not generating, and an agent waiting on a permission prompt is not generating either. Rank
it high and every blocked pane reads as idle.

#### Suppression

`skip_state_update = true` makes a rule match without setting a state, leaving whatever was
there before. This is for UI that reads like a prompt but is not one — a transcript viewer or
a model picker both contain the words a permission dialog does.

#### Predicates

Prefer `contains`: a list of case-insensitive substrings, **all** of which must be present.
No regex escaping to get wrong, which is where most pattern bugs come from. `regex` and
`line_regex` are lists where **any** may match. `any` / `all` / `not` take nested predicates,
so a rule can say "contains this, and one of those, but not that".

```toml
[[rules]]
id = "permission_prompt"
state = "blocked"
priority = 900
region = "whole_recent"
contains = ["do you want to"]
any = [
  { line_regex = ['^\s*❯?\s*\d+\.\s*yes\b'] },
  { contains = ["no, and tell claude"] },
]
```

An empty predicate matches nothing, and a rule with no conditions is rejected at parse time —
a rule that fired on everything would be worse than a missing rule.

#### Truncation

Agents elide their own status lines to fit. Claude renders `· esc to interrupt` as
`· esc to inte…` at around 50 columns and drops it entirely below about 30. So screen rules
match a short prefix (`esc to int`) rather than a whole phrase, and the title carries the
load when the grid cannot.

### Why hooks are worth installing

Screen detection can only read what is on screen. Claude Code appends `esc to interrupt` to
the end of a long status line — in a pane narrower than about 60 columns that marker is
truncated away, and a working agent reads as idle. Hooks do not care how wide your panes are.

## Diagnosing

```sh
horde agent explain reviewer
```

Prints the exact snapshot detection is matching against, which manifests claim the pane,
which rule fired, whether hooks currently hold authority, and the resulting state. This is
the first thing to run when a state looks wrong.

```sh
horde roster --json
```

`authority` tells you which tier decided: `hook` or `screen`. `reason` names the rule.

## Naming

An agent's name is how you address it in `horde send`. It defaults to the agent kind,
uniquified — `claude`, `claude-2`, `claude-3`. Set something meaningful:

```sh
horde spawn --cmd claude --name reviewer
horde pane rename 3 reviewer          # or afterwards
```

An explicit pane name always wins over the auto-generated one, and renaming a pane
re-addresses its agent.

## Restoring after a restart

With `agents.restore = true` (the default), an agent whose integration reported a session id
comes back resumed — `claude --resume <id>` — after a daemon restart. Without a session id
it comes back as a shell, because starting a fresh agent unbidden is presumptuous.
