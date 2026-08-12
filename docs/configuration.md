# Configuration

`~/.config/horde/config.toml`. Everything is optional — horde runs with no config file at
all.

Not `~/Library/Application Support` on macOS: that path puts a space in the socket path and
eats into the ~104 byte `AF_UNIX` limit. `XDG_CONFIG_HOME` is honoured if set.

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
scrollback = 10000
shell = "/bin/zsh"          # defaults to $SHELL

[theme]
# horde · tokyo-night · catppuccin · gruvbox · terminal
name = "horde"

[theme.custom]
accent = "#7ee2c0"
blocked = "#ff7b72"
working = "#f0c674"
# any of: accent accent_alt bg panel_bg title_bg text text_dim text_faint border
#         border_focus working blocked done idle unknown ok warn error selection fg cursor
# accepts #rgb, #rrggbb, rgb(r,g,b), or an ANSI colour name

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
force_inject = false        # deliver messages even mid-stream — see below
task_nudge = true           # tell an idle agent when the board has work

[notifications]
delivery = "horde"          # horde · system · off
command = "~/bin/horde-ping"  # run when something needs you and nothing is attached

[triggers]
unattended = false          # master switch: no rule fires until this is on
max_spawned = 2             # agents horde may run that it started itself (0–16)

[keys]
zoom = "prefix+f"
detach = "ctrl+alt+q"       # a modified chord binds directly
close_pane = "none"         # unbind
```

Run `horde keys` for every rebindable action name.

## `theme = "terminal"`

Follows your terminal's own ANSI palette, and passes cell colours that are still "default"
through untouched, so panes look exactly as they would outside horde.

## `agents.force_inject`

Off by default, and worth leaving off. On, it removes the state gate that holds messages
destined for a busy or blocked agent. Against a `blocked` agent that means your message
answers its open permission prompt. See [orchestration §4](orchestration.md).

## `agents.task_nudge`

On by default. The task board is pull-based — nobody assigns work, whoever is free claims it.
But an idle agent has no reason to look at a board it was never told about, so horde tells one:
when there are open tasks and an agent is free, it receives a single `[horde]` line naming
`horde task claim`.

It stays a nudge, not an assignment. `claim` is still first-come-first-served, so nothing about
who ends up with which task depends on who got told.

Three limits keep it from wasting turns, and each exists because the obvious version did:

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

`delivery = "system"` now also fires from the daemon while you are detached. That works because
the daemon inherits your GUI session; if you started horde over SSH with no such session,
macOS has nowhere to post a notification and `command` is the sink that still works.

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
