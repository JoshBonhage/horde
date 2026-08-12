# Keys and mouse

Prefix is `ctrl+b`, configurable. `ctrl+b ?` lists the live keymap, which is the
authoritative version — this page describes the defaults.

## Panes

| Key | Action |
|---|---|
| `\|` or `%` | split right |
| `-` or `"` | split down |
| `h` `j` `k` `l` | move focus — resolved by geometry, not tree position |
| arrows | same |
| `H` `J` `K` `L` | resize |
| `ctrl+h/j/k/l` | swap panes |
| `z` | zoom / unzoom |
| `x` | close pane |
| `,` | rename pane (also renames its agent) |
| `[` | scrollback |

## Tabs and spaces

| Key | Action |
|---|---|
| `c` | new tab |
| `n` `p` | next / previous tab |
| `1`–`9` | go to tab by position |
| `X` | close tab |
| `S` | new space |
| `(` `)` | previous / next space |
| `s` | space switcher |

## Agents and panels

| Key | Action |
|---|---|
| **`a`** | **jump to the next agent that needs you** |
| **`D`** | **what happened while you were away** |
| `e` | toggle sidebar |
| `b` | toggle bus drawer |
| `g` | command palette |
| `.` | settings |
| `?` | keys |
| `d` | detach — agents keep running |
| `ctrl+b` | send the prefix itself to the pane |

`ctrl+b a` is the one that earns its keep once you have more than two agents: it walks the
queue of agents that are `blocked` or `done`, so you never hunt for the one waiting on you.

`ctrl+b D` opens the digest — the same report as `horde digest`, in a scrollable panel
(`↑↓`/`j k` to move, space to page, any other key to close). Opening it counts as looking, so
the window advances and the next digest starts where this one ended. Nothing to report says so
in a toast rather than opening an empty panel.

## Mouse

Left-click a pane or a sidebar row to focus it, click a tab to switch, scroll to page back
through scrollback.

**Right-click anything** for a context menu built from what is under the cursor:

| Right-click | You get |
|---|---|
| a pane | split, start an agent, run a command, zoom, rename, copy visible text, layout, close |
| an agent pane or agent row | the above plus **send message** |
| a space row | focus, new tab here, rename, new space, close space |
| a tab | focus, rename, layout, new tab, close tab |
| anywhere else | new space, new tab, start agent, layout, toggle panels, jump to attention |

Every entry shows its keyboard equivalent, so the menu teaches the keys rather than replacing
them. `›` marks a submenu. Arrows or `j`/`k` navigate, `enter` activates, `esc` steps out of a
submenu and then closes.

## Modes

Three input modes, shown as a chip on the left of the status bar:

- **terminal** — keystrokes go to the focused pane. The chip shows your prefix key.
- **prefix** — you pressed the prefix; the next key is a horde binding. The chip reads
  `PREFIX` and the bar lists the common bindings.
- **overlay** — `MENU`, `SETTINGS`, `COMMAND`, `SPACE`, `INPUT`, or `HELP`. `esc` leaves.

Without that chip there is no feedback that the prefix registered, which is why it is the
most prominent thing on the bar.

## Not implemented

Dragging pane borders to resize — use `H J K L`. Text selection in copy mode — right-click →
copy visible text instead. OSC 52 clipboard forwarding.
