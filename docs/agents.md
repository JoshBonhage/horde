# Agents: detection and state

horde works out what each agent is doing so the sidebar can tell you which ones need you.

## The five states

| State | Glyph | Meaning |
|---|---|---|
| `working` | `◐` (animated) | actively running a turn |
| `blocked` | `◍` | waiting on a human decision — an approval or permission prompt |
| `done` | `●` | finished, and you have not looked yet |
| `idle` | `○` | at its prompt, and you have seen it |
| `unknown` | `◌` | horde cannot tell confidently |

`blocked` is deliberately strict. It is set only when a visible approval, question, or
permission UI is matched. **Silence is never treated as blocked** — an agent that has gone
quiet is not the same as one asking you something, and conflating them would make the
attention count useless.

`done` is derived, not reported. A `working → idle` transition while you are not looking at
the pane becomes `done`, and focusing or typing in the pane clears it back to `idle`. That is
the "finished while you were away" signal.

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
2. the **foreground process name** from `ps` — a definite answer
3. the **command the pane was started with** — what we were asked to run
4. **screen patterns** — a guess, and the only ambiguous signal

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
