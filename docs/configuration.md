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

[notifications]
delivery = "horde"          # horde · system · off

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

## Bad values do not stop startup

A malformed key spec, an unknown theme, an unparseable file — each produces a warning toast
and falls back to the default. horde starting with a complaint beats horde not starting.

## Detection overrides

`~/.config/horde/agents/<name>.toml` replaces a bundled manifest wholesale. See
[agents](agents.md).
