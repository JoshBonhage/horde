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
| `r` | redraw — force every program to repaint at its current size |

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
| `E` | give the sidebar the keyboard — see below |
| `o` | the roster: every project and agent, full screen |
| `f` | filter the agent list — see below |
| `P` | pin the focused agent to the top of the sidebar |
| `b` | toggle bus drawer |
| `g` | command palette |
| `.` | settings |
| `?` | keys |
| `d` | detach — agents keep running |
| `ctrl+b` | send the prefix itself to the pane |

`ctrl+b a` is the one that earns its keep once you have more than two agents: it walks the
queue of agents that are `blocked` or `done`, so you never hunt for the one waiting on you.

### Walking the sidebar

`ctrl+b E` hands the keyboard to the sidebar so its list can be walked with single keys. The
status bar shows a **SIDEBAR** chip while it has them, because a panel that quietly stops your
keystrokes reaching a pane is the worst thing it could do.

| Key | Action |
|---|---|
| `j` `k`, `↓` `↑` | move a row |
| `pgdn` `pgup` | move a page |
| `g` `G` | first / last row |
| `enter` | go to that space or agent, **and leave the panel** |
| `h` `l`, `←` `→` | fold / unfold a group |
| `space` | fold or unfold, either way |
| `p` | pin or unpin the agent under the cursor |
| `f` | step the filter |
| `r` | show every agent doing the same job as this one |
| `esc` `q` | give the keys back |

`enter` acts and exits in one keystroke, so the common path — glance, jump, type — never
leaves you parked in a mode you then have to notice and get out of. Any key the panel does not
recognise is ignored rather than passed through: falling through would type `x` into an agent
you were only looking at.

A group you fold stays folded across a detach, a restart, and a `horde upgrade` — it is a
decision about your session, not a property of the client that happens to be drawing it. The
cursor is not: where it happens to be does not deserve to survive being away.

### Filtering the list

`ctrl+b f` steps the agent list through a filter, and the heading names the one in force —
a filtered list that does not say it is filtered reads as a broken one.

```
all → needs you → working → here → all
```

The cycle ends back at `all`, so the same key is always also the way out. The footer counts
stay session-wide whatever the filter, on purpose: a lens that also silenced them would hide
the very thing you filtered away.

`here` is the focused space only.

Pressing `r` on an agent filters to *its role* — every `reviewer` across all your projects,
which is the question the space tree cannot express at all and the reason roles exist. You
are pointing at the role when you press it, so it needs no explaining; `f` steps back out to
`all` from there.

### The roster

`ctrl+b o` drops the panes and gives the whole terminal to one view: every project as a card,
with its agents, their states, what they are doing, and the directory it lives in. Up to
three columns, one on a narrow terminal.

It shares the sidebar's cursor, so opening it lands you where you left off and jumping from it
leaves the sidebar in agreement. `j`/`k` move, `enter` jumps and closes, `p` pins, `f`
filters, `esc` closes. Click a row to jump straight to it.

This is the view for "what is running everywhere" — the sidebar answers that in fourteen
columns, and the roster answers it with room to spell things out.

`ctrl+b D` opens the digest — the same report as `horde digest`, in a scrollable panel
(`↑↓`/`j k` to move, space to page, any other key to close). Opening it counts as looking, so
the window advances and the next digest starts where this one ended. Nothing to report says so
in a toast rather than opening an empty panel.

## Mouse

Left-click a pane or a sidebar row to focus it, click a tab to switch, scroll to page back
through scrollback.

### Selecting text

**Drag across a pane to highlight, and it copies when you let go.** No key to press, nothing to
confirm.

- The selection belongs to **one pane** and stops at its edge, so dragging across a split never
  welds two panes' output together the way a plain terminal selection does.
- It is **line-oriented**: from the middle of one line to the middle of another takes everything
  in between, and trailing blanks are trimmed off each line.
- A **click that does not move** selects nothing and leaves your clipboard alone — that is how
  you focus a pane, and it should not cost you what you had copied.
- The highlight clears when you type, scroll, or start another selection.
- Wide glyphs are counted in the columns they are actually drawn in, so a selection after CJK
  text or an emoji is not shifted.

For a program running its own mouse handling (`vim`, `htop`, anything with mouse mode on), the
mouse goes to the program. **Hold shift to take it back** and select instead. Some terminals
intercept shift-drag for their own native selection before horde sees it — that still copies, it
just uses the terminal's idea of where panes are rather than horde's.

Right-click a pane and choose **copy visible text** to take the whole pane instead of a
selection.

**Right-click anything** for a context menu built from what is under the cursor:

| Right-click | You get |
|---|---|
| a pane | split, start an agent, run a command, zoom, rename, copy visible text, layout, close |
| an agent pane or agent row | the above plus **role**, **pin to top**, and **send message** |
| a space row | focus, new tab here, rename, **colour**, **collapse**, new space, close space |
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
