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

Without hooks, horde identifies the foreground process and matches regexes against the live
bottom of the pane buffer.

**Which agent is in a pane** is decided in order of how much the signal can be trusted:

1. a fresh **hook report** — the agent said so itself
2. the **foreground process name** from `ps` — a definite answer
3. the **command the pane was started with** — what we were asked to run
4. **screen patterns** — a guess, and the only ambiguous signal

The order matters because agent UIs share phrases. `esc to interrupt` appears in Claude Code,
Codex, and Cursor Agent alike, so a `detect` list containing it makes several manifests claim
the same pane — and if the winner is decided by hash order, the pane flickers between names
between one scan and the next. Every bundled `detect` list is now specific to its own agent,
and the iteration is sorted.

Bundled manifests live in `agents/*.toml` and are compiled into the binary. Override one
wholesale by dropping a file at `~/.config/horde/agents/<name>.toml`.

```toml
name = "claude"
processes = ["claude"]

# Patterns proving *this* agent's UI is on screen. Must be unique to it: anything generic
# makes two manifests claim one pane.
detect = ['Claude Code', '\? for shortcuts', 'shift\+tab to cycle', '⏵⏵']

# First matching rule wins. Order matters — blocked before working before idle.
[[rules]]
name = "blocked-permission"
state = "blocked"
any = ['Do you want to (proceed|make this edit)', '❯\s*\d+\.\s']

[[rules]]
name = "working-esc-to-interrupt"
state = "working"
within = 4              # the live status area only, not scrollback
any = ['esc to int']    # short prefix: survives the pane eliding the line

[[rules]]
name = "idle-prompt"
state = "idle"
within = 6
any = ['^❯', 'shift\+tab to cycle']
```

Patterns are single-quoted TOML literals so regex backslashes need no escaping. Matching is
case-insensitive and `^`/`$` bind to line boundaries. A rule fires when at least one `any`
matches, every `all` matches, and no `none` matches. An unmatched screen falls back to
`idle` with a labelled reason rather than `unknown` — a known agent sitting at a prompt horde
does not recognise is far more likely idle than indeterminate.

The snapshot comes from the **live bottom** of the buffer, not the scrolled viewport, so
scrolling back never changes what horde thinks an agent is doing.

#### `within` — and why a stuck state usually means a missing one

`within = N` restricts a rule to the last N lines. Use it for anything describing what an
agent is doing *right now*.

The snapshot is ~40 lines, which includes what the agent **said**, not only its live chrome.
An agent that printed "Thinking…" ten minutes ago still has those words on screen, so a
`working` rule without `within` keeps firing forever — the spinner never stops and the
elapsed timer counts up indefinitely. Scoping the rule to the bottom few lines, where the
status area lives, fixes it.

#### Truncation

Agents elide their own status lines to fit the pane. Claude renders
`· esc to interrupt` as `· esc to inte…` at around 50 columns, and drops it entirely below
about 30. So:

- **match a short prefix**, never a whole phrase — `esc to int` survives elision
- below roughly 30 columns there is nothing left to match, and **only hooks can tell you
  what an agent is doing**

This is the single best reason to run `horde integration install claude`.

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
