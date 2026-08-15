# Configuration

`~/.config/horde/config.toml`. Everything is optional — horde runs with no config file at
all.

Not `~/Library/Application Support` on macOS: that path puts a space in the socket path and
eats into the ~104 byte `AF_UNIX` limit. `XDG_CONFIG_HOME` is honoured if set.

A complete working file is committed as
[`config.example.toml`](../config.example.toml) — copy it to `~/.config/horde/config.toml` and
edit. It carries the model and handover blocks, which are the tedious ones to reconstruct from
this page.

## The settings page

`ctrl+b .` opens settings with categories down the left: **Appearance, Keybindings, Agents,
Notifications, Terminal, About**.

`tab` switches category, `↑`/`↓` moves, `←`/`→` changes a value. Changes apply immediately
and persist.

To rebind a key: select the action, press `enter`, then press the key you want. A bare key
becomes a prefix binding; a modified chord becomes a direct one — binding a bare printable
key directly would swallow ordinary typing. A chord another action already owns is refused,
naming the conflict, rather than leaving two actions fighting over it.

Writing goes through `toml_edit`, so comments, key order, and formatting in a hand-edited
file survive. A settings screen that reformats the file you maintain by hand is worse than no
settings screen.

## Full example

```toml
prefix = "ctrl+b"
leader = "ctrl+space"       # opens the multi-key leader table
scrollback = 10000
shell = "/bin/zsh"          # defaults to $SHELL

[theme]
# horde · tokyo-night · catppuccin · gruvbox · terminal
name = "horde"
# Colours projects are tinted with, by position. Six slots; a short list replaces only the
# ones you name. A space stores which *slot* it uses, not a colour, so retinting here moves
# every project on that slot at once — see below.
space_accents = ["#79c0ff", "#d2a8ff"]

[theme.custom]
accent = "#7ee2c0"
blocked = "#ff7b72"
working = "#f0c674"
# any of: accent accent_alt bg panel_bg title_bg text text_dim text_faint border
#         border_focus working blocked done idle unknown serving ok warn error selection
#         fg cursor
# accepts #rgb, #rrggbb, rgb(r,g,b), or an ANSI colour name

# Roles you have named, and how each is drawn. Declaring one styles it; it does not permit
# it — `horde pane role %2 anything` works whether or not it appears here.
[[roles]]
name = "reviewer"
color = "#79c0ff"
glyph = "◈"                 # must be one cell wide

[[roles]]
name = "builder"
color = "#7ee787"

[ui]
sidebar = true
sidebar_width = 24          # 14–60
bus = false                 # ctrl+b b toggles it
bus_width = 30              # 18–70
pane_titles = true
tab_bar = true
status_bar = true
animate = true              # spinners for working agents

[agents]
restore = true              # resume agents after a daemon restart
detection_lines = 40        # rows of the live buffer detection reads
force_inject = false        # deliver messages even mid-stream
task_nudge = false          # tell an idle agent when the board has work

[notifications]
delivery = "horde"          # horde · system · off
command = "~/bin/horde-ping"  # run when something needs you and nothing is attached

[triggers]
unattended = false          # master switch: no rule fires until this is on
max_spawned = 2             # agents horde may run that it started itself (0–16)

[vault]
home = "~/notes"            # the vault that is always there, whatever project you are in
dir = "notes"               # a project's own notes directory, when it has one
enabled = true

[keys]
zoom = "prefix+f"
detach = "ctrl+alt+q"       # a modified chord binds directly
leader_window_zoom = "leader w f"   # a leader sequence, keys separated by spaces
close_pane = "none"         # unbind
```

Run `horde keys` for every rebindable action name.

## Languages

Syntax colouring is compiled in rather than configured, because a tree-sitter grammar is C
that has to be built with the binary. `horde status` reports which languages a build has.

| Build | Knows |
|---|---|
| default | markdown, rust, typescript, tsx, javascript, python, json, toml, bash |
| `--no-default-features` | nothing — every file is plain text |
| `--no-default-features --features lang-rust,lang-markdown` | just those two |

The full set costs roughly 6.6 MB of binary, unevenly: typescript and tsx are 2.8 MiB of it
and json is 8 KiB. Colours come from the theme, so nothing needs setting per language.

Three binding forms, and which one you get depends on the spec:

| Spec | Reached by |
|---|---|
| `prefix+x` | the prefix, then `x` |
| `ctrl+alt+q` | the chord on its own, no prefix |
| `leader w f` | the leader, then `w`, then `f` — up to three keys |

A bare printable like `"f"` is refused as a direct binding, because it would shadow ordinary
typing. After the leader it is fine, and expected: nothing typed there was going to reach a
program. Leader sequences stop at three keys — past that a binding is a menu, and a menu
should be something you read rather than something you memorise.

## Project colours

Every space is assigned an accent when it is created — the least-used slot, so two projects
next to each other differ in colour as well as name without you asking for either. It shows
in the tab bar, on the space's sidebar dot, and on the borders of every pane in that space.

A space stores a **slot number**, not a colour. That is what lets a theme change repaint all
of them together: chrome colours are resolved by the client from whichever theme it is
running, so a stored `#79c0ff` would leave one project painted in the old palette while
everything around it moved. It also means `space_accents` in `[theme]` is the right place to
put a literal colour — config outlives `state.json`, so your choices survive `horde stop`.

The focused pane keeps its own border colour rather than the project's. Which pane has the
keyboard is the one thing that border exists to answer, and it should not become a question
of hue.

```sh
horde space accent api-refactor 3    # a specific slot
horde space accent api-refactor      # step to the next one
```

## Roles

A role is what a pane is *for*: `reviewer`, `builder`, `docs`. Three names meet on an agent
pane and none substitutes for another — the pane's name is how you address it, the agent's
kind is which program was detected, and the role is the job. Only the role recurs across
projects, which is what makes it worth grouping by.

```sh
horde pane role %2 reviewer
horde pane role %2                   # clears it
horde role list                      # who is doing what, across every project
```

Names are normalised — trimmed, lowercased, spaces and underscores folded to `-`, capped at
16 characters — so `Code Reviewer` and `code_reviewer` are one role rather than three.
Without that, roles would fragment and stop being the thing they exist to be.

An undeclared role still works, and gets a glyph of `◆` and a colour derived from its name,
stable across runs and projects. `[[roles]]` only chooses how a role looks.

## `theme = "terminal"`

Follows your terminal's own ANSI palette, and passes cell colours that are still "default"
through untouched, so panes look exactly as they would outside horde.

## `agents.force_inject`

The bus is on. `bus.send`, `bus.reply` and `bus.broadcast` all deliver, by hand, from a trigger,
or from the board's nudge.

The rest of this section is how it behaves when the bus is back on.

Off by default, and worth leaving off. On, it removes the state gate that holds messages
destined for a busy or blocked agent. Against a `blocked` agent that means your message
answers its open permission prompt. See [orchestration §4](orchestration.md).

## `agents.task_nudge`

**Off by default.** The board works by hand without it; this is the half that acts unasked,
and it is worth watching a board behave for a day before switching it on.

The task board is pull-based — nobody assigns work, whoever is free claims it.
But an idle agent has no reason to look at a board it was never told about, so horde tells one:
when there are open tasks and an agent is free, it receives a single `[horde]` line naming
`horde task claim`.

It stays a nudge, not an assignment. `claim` is still first-come-first-served, so nothing about
who ends up with which task depends on who got told.

Five limits keep it from wasting turns or reaching the wrong agent, and each exists because
the obvious version did not have it:

- **One project at a time.** A task belongs to the space it was added in, and is only offered
  to agents in that space. Without this the board inverts the moment you have two projects
  open: work added in one repository is handed to an idle agent sitting in another, which
  claims it and starts editing the wrong tree.
- **Volunteers only.** An agent is offered work after `horde task work`, or if it was spawned
  with `--board`, and never otherwise. Before this every idle agent counted as a worker,
  including the one you opened to think with.
- **Never more agents than there is work.** One task wakes one agent, five tasks wake up to
  five. The first version woke one agent *per detection pass*, which over a few seconds meant
  every idle agent was told about a single task.
- **Once per idle period.** Adding ten tasks at once produces one nudge, not ten. An agent that
  ignores it is not asked again until it has actually done something.
- **`done` agents are left alone unless they work the board.** A finished agent normally holds
  a result you have not read. One that has completed a board task has its result recorded on
  the board instead, so it stays in the loop — without this, each agent took exactly one task
  and then went quiet.

Turn it off with `task_nudge = false`, or from the settings page under Agents, and the board
becomes purely manual: agents only find work when they look.

Open tasks stop being offered after a day (`tasks::STALE_AFTER`). A week-old task is not work
waiting for an agent, it is something you forgot about, and offering it to a fleet on the next
restart is how a quiet morning turns into archaeology. It stays on the board, still readable
and still claimable by id — stale is not deleted. `horde task clear` wipes a project's open
work outright.

## `[handover]` — hand over before you run out

```toml
[handover]
warning = ["Approaching usage limit"]
profile = "free"
```

When an agent's pane shows `warning`, horde tells that agent to hand over — **while it still
can**. This is the case a metered plan creates: once an agent is out of budget it can do nothing
at all. It cannot spawn a successor, cannot write a note, cannot answer a question. So the only
moment a handover can be arranged by the agent itself is *before*, and the only thing reliably
watching is whatever reads the screen.

horde does not spawn the successor. **The agent does**, because a brief it writes about its own
half-finished work beats anything reconstructed from a screen. What horde contributes is
noticing, and spending the agent's last usable turn on the handover rather than on work it will
not finish.

The default instruction tells it to write `.horde/handoff-<name>.md`, commit or stash so the tree
is not left mid-edit, then run `horde spawn --profile <profile> --brief "read that file first"`.
Override it with `instruct`, where `{name}` and `{profile}` are substituted.

Told exactly once per agent: the warning stays on screen, and repeating the instruction would
interrupt the handover it is asking for.

### When no warning ever came

```toml
[handover]
exhausted = ["reached your usage limit"]
max_chain = 3
```

The net under `warning`. An agent that stopped mid-sentence never got the chance to write its own
note, so horde spawns the successor itself and composes the brief from what it watched: which
agent this replaces, where it was working, what git thinks changed, whether the tree was left
dirty, and the last of its screen. If the agent *did* leave a `.horde/handoff-<name>.md`, the
brief points at that first — it is better than anything horde can reconstruct.

The successor is named `<name>-next`, starts on `profile`'s first model, counts against
`triggers.max_spawned`, and is journalled so `horde digest` records that the work changed hands.

`max_chain` (3 by default) ends a lineage that keeps running out. If three agents in a row have
run out, the answer is not a fourth.

### What Claude Code actually says

The limit message is quoted verbatim in
[anthropics/claude-code#9236](https://github.com/anthropics/claude-code/issues/9236) and
[#5977](https://github.com/anthropics/claude-code/issues/5977):

```
Claude usage limit reached. Your limit will reset at 3pm (America/New_York)
```

So for Claude Code:

```toml
[handover]
exhausted = ["usage limit reached", "Your limit will reset at",
             "limit reached, resets", "limit reached - resets"]
warning   = ["Approaching 5-hour limit", "Approaching usage limit"]
```

The `exhausted` strings are well sourced. **The warning wording is not** — Anthropic's help
centre describes the behaviour without quoting it, so those two came from third-party write-ups
and should be treated as a guess until you see one. Both are one-line edits, and `horde.log`
records `told to hand over` and `ran out; started` so you can tell which fired.

horde ships no defaults for either, because the wording belongs to whichever agent you run and
inventing one produces a feature that either never fires or fires on the wrong thing.

Matching ignores whitespace, so a phrase still matches when the terminal wrapped it — in a narrow
pane an agent will split `esc to interrupt` into `in`/`te`/`rr`/`up`/`t` down five lines, and a
literal match would find none of it.

## `agents.board`

```toml
[agents]
board = false
```

Whether the shared task board accepts anything at all. On by default; `false` refuses every
`task.*` call at the socket **and** stops a scheduled `--task` rule from placing work, which
reaches the board directly rather than through the socket and would otherwise keep filling a
board you had closed.

Deliberately separate from the bus. Messaging is agents talking to each other; the board is
agents *taking work* nobody watched them take. Wanting the first without the second is a coherent
position, and before this switch the only way to hold it was to hope nobody tried.

## `agents.max_fleet`

How many live panes agents may have started between them. Six by default.

Separate from `triggers.max_spawned`, which bounds what horde starts with nobody present. This
bounds what an agent starts while you are sitting there, which is a different risk: not "is
anyone watching" but "an agent in a loop opens panes until the machine gives up". A lead agent
building a team is the intended use, so the number is a working team rather than a token
allowance. It survives `horde upgrade`, because a cap you can reset by restarting is not a cap.

## `notifications.command`

Every other way horde tells you something needs you assumes a client on screen — a toast, the
sidebar, the digest you read when you get back. That is the one case where being told adds
nothing, because you are already looking. So there is a second path, and it runs **only while
nothing is attached**: the daemon is the part still awake for the hour that actually needs a
notification.

`command` runs through `sh -c`, so it can be a path or a pipeline, and it gets the news twice
over:

- **`$1`** — the one-line summary, the same line the reattach toast uses:
  `while you were away: 1 agent needs you, 2 tasks done`
- **stdin** — the full `horde digest --json` payload, for a script that decides what to do
  rather than just forwarding.

A two-line script is the whole integration, which is the point — horde has no HTTP client and
nowhere to keep a token:

```bash
#!/bin/sh
# ~/bin/horde-ping
curl -s -F "token=$PUSHOVER_TOKEN" -F "user=$PUSHOVER_USER" \
     -F "message=$1" https://api.pushover.net/1/messages.json
```

Telegram, ntfy, Slack, `mail` — same shape. Setting `command` is itself the opt-in, so it runs
under `delivery = "horde"` too; `delivery` says where horde's *own* notifications go. `off`
silences everything, including this.

**What earns an alert.** The digest's own top facts, and nothing else:

- an agent has wanted a human, or sat on a finished turn nobody has read, for a full minute
- the task board emptied — the fleet is done

**Four rules keep it worth reading**, and they matter more than the sink does:

- **Detached only.** No overlap with the in-app toast, so nothing arrives twice.
- **Settled facts only.** A board worker is briefly `done` between every task; a minute of
  waiting is what separates a real stop from a state passed through.
- **One ping per wait.** An agent stuck for an hour is reported once. One that blocks, gets
  answered, and blocks again is reported twice — those are different waits.
- **One ping per five minutes**, carrying the whole window in one line. A notifier you learn to
  ignore is worse than none.

Being told is not the same as having looked, so an alert does **not** advance the digest window.
The report waiting when you get back is still the whole story.

Every alert is recorded in the journal, so "was I told, and when" is answerable afterwards
rather than a matter of trusting that it worked.

`delivery = "system"` now also fires from the daemon while you are detached. It resolves to
`osascript` on macOS and `notify-send` on Linux, and to nothing at all under WSL, where every
candidate costs a dependency worth more than the setting. It also needs a GUI session to post
into, which horde over SSH does not have.

Where there is no sink, the daemon logs the reason once rather than failing quietly on every
alert — and `command` is what still works. Under WSL that is the route, because any Windows
executable on `$PATH` is reachable from the command hook and `$1` is the summary:

```toml
[notifications]
# Whichever Windows-side toaster you have. Both of these need installing first —
# BurntToast is a PowerShell module, wsl-notify-send is a single .exe.
command = 'powershell.exe -NoProfile -Command "New-BurntToastNotification -Text horde, \"$1\""'
# command = 'wsl-notify-send.exe --category horde "$1"'
```

Neither ships with Windows, which is the whole reason horde does not try to pick one for you.
`interop.appendWindowsPath` must also still be on in `wsl.conf`, or no `.exe` is on `$PATH` at
all.

## `[env]` — what every pane gets

```toml
[env]
OPENROUTER_API_KEY = "sk-or-..."
OPENCODE_CONFIG = "/home/you/.config/opencode/opencode.json"
```

Handed to every pane horde opens, applied last so it overrides horde's own defaults. This is how
a provider key reaches an agent.

Inheriting the key from the daemon's environment looks equivalent and is not. The daemon is
`setsid`'d away from whichever shell started it, so a key exported in `.bashrc` reaches it only
when horde was started from an interactive shell — and a daemon started any other way gets a thin
environment and an agent that cannot authenticate, with nothing on screen explaining why. Naming
it here removes the guess. It matters most under WSL, where the daemon's ancestry is least
predictable.

**Values are treated as secrets.** They are never written to the log, the journal, `horde status`
or `state.json`. The cost of that is real and deliberate: a mistyped key produces an agent that
fails to authenticate with nothing in horde's log to explain it. A key in a log file outlives
every session that could have used it.

## `[models]` — a list to work through

```toml
[models.free]
cmd = "opencode --model openrouter/{model}"
order = [
  "qwen/qwen3-coder:free",
  "deepseek/deepseek-chat-v3.1:free",
  "z-ai/glm-4.5-air:free",
]
```

```sh
horde spawn --profile free --name builder
```

`{model}` is replaced with an entry from `order`; `--profile` starts at the head of the list and
beats `--cmd` when both are given. horde keeps no catalogue of its own — this is your list, and
horde's only opinion about it is which entry an agent is currently on.

The list does not wrap. A fleet that has burned through every free model should stop and say so;
rotating forever turns "the free tier does not support this workload" into an agent that looks
busy and achieves nothing.

### Switching automatically

```toml
exhausted = ["Rate limit exceeded", "rate_limit_exceeded", "429"]
switch = "/models openrouter/{model}"
```

With both set, horde moves an agent to the next model on its own. `exhausted` is **screen text**,
not a status code — horde has no HTTP client, so it reads the provider's error the same way it
reads every other agent state: as words rendered into the pane. The defaults match what
OpenRouter returns and opencode prints; change them if your agent words it differently.

`switch` is typed into the *running* agent, so its session survives — the plan it had formed and
everything it had read are still there. Restarting the agent instead would cost more than the
rate limit did. Leave `switch` unset and horde reports the model is spent without touching it.

Every switch is logged and journalled, so `horde digest` says which model did which work. That
matters more than it sounds: the failure mode of this feature is waking up to work done by a
model you did not choose, with nothing saying so.

A switch is ignored for 30 seconds afterwards. The error that triggered it is still in the
scrollback, and without the pause one rate limit would walk an agent through the whole list.

An unknown profile name is refused rather than defaulted. Quietly falling back to `claude` when
you asked for the free tier would spend the wrong budget on the wrong provider and look like it
worked.

Full reasoning, and where automatic switching goes next: [PLAN-models.md](../PLAN-models.md).

## `triggers.unattended`

Off by default, and the one switch that changes what horde *is*. On, scheduled rules may put work
on the board while nobody is watching; off, they can be added and listed but never fire.

```bash
horde trigger add --at 09:00 --task "review yesterday's diff"
```

The sidebar footer shows `◈ 2 triggers armed` whenever anything could fire, because that horde is
allowed to act on its own should be visible rather than remembered. Full reference:
[unattended](unattended.md).

## Bad values do not stop startup

A malformed key spec, an unknown theme, an unparseable file — each produces a warning toast
and falls back to the default. horde starting with a complaint beats horde not starting.

## Detection overrides

`~/.config/horde/agents/<name>.toml` replaces a bundled manifest wholesale. See
[agents](agents.md).
