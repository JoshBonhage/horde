# Keys and mouse

Prefix is `ctrl+b`, configurable. `ctrl+b ?` lists the live keymap, which is the
authoritative version — this page describes the defaults.

There is a second way in: the **leader**, `ctrl+space`, which opens a table of multi-key
sequences instead of single keys. The prefix is for the multiplexer's own verbs and stays
one keystroke deep; the leader is where everything else goes, grouped by what it is for.
See [The leader](#the-leader).

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
| `0` | the start screen — tabs are 1–9, so home is 0 |
| `N` | this project's notes |
| `w` | write a new note |
| `F` | this project's files |
| `G` | the link graph |
| `d` | detach — agents keep running |
| `ctrl+b` | send the prefix itself to the pane |

`ctrl+b a` is the one that earns its keep once you have more than two agents: it walks the
queue of agents that are `blocked` or `done`, so you never hunt for the one waiting on you.

## The start screen

**Opening horde lands here, every time.** Arriving is the moment you want the state of
things — which agents need you, which projects are live, which ones you had open last time
— rather than whichever pane happened to be focused when you left.

The daemon is untouched by this. Agents keep running whether or not anyone is looking, which
is the whole point of a daemon; it is only the *view* that resets, and `esc` is one keystroke
from the terminal. `ctrl+b 0` or `ctrl+space d` brings it back whenever you want.

| Key | Action |
|---|---|
| `j` `k`, arrows | walk the rows |
| `enter` | open the row — focus a live project, reopen a remembered one |
| `p` or `P` | the project picker |
| `n` | new project |
| `o` `D` `.` `?` | roster · digest · settings · keys |
| `esc` | drop into the terminal |
| `q` | detach |

A live project shows what is running in it (`main*  2 agents  ◍1`); a remembered one says
`resume` and how long ago. The row tells you which before you press anything, so `enter`
never starts a second copy of a project you already had open.

Set `dashboard = false` under `[ui]` to attach straight into the terminal instead.

## Opening a project

`enter` on a project — on the start screen, or `ctrl+b F` any time — shows you **the
project**: its files, as a tree, filtered as you type. `enter` on a file opens it for editing
inside horde. `ctrl+t` gives you a terminal in it.

That way round on purpose. Opening a project is usually about a file, and the multiplexer is
a keystroke away rather than the thing you go through to reach anything else.

Folders are closed until you ask for them, so a project arrives as its shape rather than as
every file it contains. `enter` or `→` opens one, `←` closes it, and typing a query opens
whatever holds a match — a folder collapsed over the thing you are looking for is a folder
lying to you. Directories come before files at every level, which is how a project reads.

The listing is what you wrote, not what the machine made: build output, vendored code,
version history, and binaries are skipped. It is not a full `.gitignore` implementation —
just the directories that are output in every language — and it stops at a few thousand
files rather than pretending to offer a list nobody could pick from.

Editing a project file is the same editor notes use, minus the live preview — that is a
markdown idea and would happily read `**p` in C as bold text. Code gets **syntax colouring**
instead: comments, keywords, types, strings and numbers, in the colours of whichever theme
you are running, so a window painted in gruvbox does not have somebody else's editor inside
it.

Which languages a build understands is a property of the binary, not a setting: grammars are
compiled in. `horde status` lists them. The default build knows markdown, rust, typescript,
tsx, javascript, python, json, toml and bash, and costs about 6.6 MB for the privilege — a
build that only wants notes is

```sh
cargo build --release --no-default-features --features lang-markdown
```

which comes out around half the size.

## Notes

`ctrl+b N` opens the vault for the project you are in: every note, filtered as you type,
with what links to each one. `enter` opens the note in `$EDITOR` in a split.

### Setting up

The first time horde starts with no `config.toml`, it opens a short walkthrough: where notes
live, which languages the editor should colour, and whether horde may act while nobody is
attached. Every step has a sensible answer already chosen, so `enter` four times is a valid
way through it, and `esc` skips it entirely. It writes `config.toml` and creates the vault.

Nothing about it is one-way — everything it asks lives in config and changes whenever you
like.

**Writing a note never needs a project.** `ctrl+b w` asks for a title and drops you into
it, from wherever you are — a pane, the start screen, another note. A thought worth keeping
rarely arrives while you happen to have the right directory open.

Where it goes: the project's own vault if it has one, else the **home vault**, which is
always there. That is `~/notes` by default and `vault.home` in config. horde creates it the first time you
write a note — not before, since making directories on somebody's disk unasked is rude, and
asking for a note *is* the ask. It marks it with a `.horde-vault` file so it is found again by looking rather than
by being configured — the same trick `.obsidian/` plays, which is also why an Obsidian vault
you already keep is adopted as-is when you open a space **on** it.

Only the directory itself, or the one `vault.dir` names. horde does not go looking through
subdirectories for a vault to adopt: open a space on your home directory and it would index
whichever one it happened to find first, which is a surprise nobody asked for.

Notes are tracked, human-owned content, which is why they are never under `.horde/`: that
directory is excluded from git on purpose, and notes are the opposite of scratch.

The browser shows the vault as a **tree** — folders as headings, notes indented under them.
The cursor walks past folders, since there is nothing to do to one.

| Key | Action |
|---|---|
| any letter | filter by title, folder or tag |
| `↑` `↓` | move |
| `enter` | **read the note**, rendered |
| `ctrl+e` | **write in it**, in horde |
| `ctrl+n` | new note |
| `backspace` | delete a character of the filter |
| `esc` | close |

### Writing a note

`ctrl+e` opens the note for editing inside horde — no pane, no `$EDITOR`, no multiplexer.
It is **modeless**: typing types. This is a notes app, and a vim grammar here would make
every note a small quiz about which mode you are in before a keystroke means what it looks
like it means.

It is also a **live preview**. Markers do their job and get out of the way as you type:
`**bold**` reads as bold, `# ` gives its line weight, `[[a link]]` loses its brackets. The
one exception is the line the cursor is on, which shows its source — hiding characters under
a cursor would make the arrow keys lie about where they are going.

| Key | Action |
|---|---|
| anything | types |
| arrows, `home` `end`, `page` | move |
| `ctrl+s` | save |
| `ctrl+r` | save and read it rendered |
| `esc` | save and go back |

Leaving saves. An editor that can lose a note because you pressed the wrong key to get out
of it is not one to trust a thought to. The status bar shows `WRITING` in its own colour,
and a `•` next to the filename while there is anything unsaved.

### Reading a note

`enter` renders the note rather than showing its source: headings carry weight, emphasis is
emphasised, wikilinks lose their brackets, code sits on its own ground, and callouts become
a coloured bar. Prose is wrapped to a readable column whatever the terminal's width.

| Key | Action |
|---|---|
| `j` `k`, `space`, `page` | scroll · `g` `G` for the ends |
| `tab` / `shift+tab` | walk the note's links — the one in play is marked `▸` |
| `enter` | follow the selected link |
| `e` | open this note in `$EDITOR` |
| `esc` | back to the browser |

Following a link is what separates a vault from a directory of files: the name is resolved
by the daemon, which is the side holding the index. A link to a note nobody has written says
so rather than opening an empty file.

A terminal has one font size, so a heading earns its weight from colour, boldness and a rule
under it rather than from being larger. Images are not rendered — that needs a terminal
graphics protocol, and is deliberately out of scope.

This is the one full-screen view with no single-key commands of its own: every printable
key types into the filter, because a view whose whole job is searching should not have `p`
mean something else.

Agents read the same index over the socket — `vault.list`, `vault.search`, `vault.read`.

## The graph

`ctrl+b G` draws the vault: a node per note, an edge per wikilink, laid out by a force
simulation until it settles and then stops. Notes that link to each other end up near each
other, so a cluster on screen is a subject in the vault.

| Key | Action |
|---|---|
| `tab` / `j` `k` | next / previous node |
| `↑` `↓` `←` `→` | pan |
| `+` `-` | zoom · `0` recentres |
| `enter` | open the note in `$EDITOR` |
| `esc` | close |

What the drawing tells you:

- **A hollow node with no colour is a ghost** — a `[[link]]` to a note nobody has written.
  They are shown rather than hidden, because the notes a vault is missing are worth seeing.
- **Colour is the cluster**: a note's first tag, or its folder when it has no tags.
- **Size is connectedness.** A bigger dot is linked to more things.
- **Above a couple of hundred edges, only the selected node's links are drawn**, and the
  header says so. Drawing all of them fills a terminal with braille and hides the shape the
  layout is there to show; the clustering still reads, because that lives in where the nodes
  sit rather than in the lines.

The layout stops moving once it settles, and stops redrawing with it — a graph left open on
screen costs no more than any other still picture.

## The leader

`ctrl+space` opens the leader table. Unlike the prefix, a leader binding can be several keys
long, so bindings live in named groups rather than competing for the same 40-odd single
letters. Press it and wait: a popup lists every key you can press next, with `+name` marking
a group that has more behind it.

| Key | Group |
|---|---|
| `space` | the finder |
| `d` | the start screen |
| `n` | notes — `n` new · `o` browse · `f` find · `g` graph |
| `g` | graph |
| `a` | agents — `a` attention · `q` approvals · `r` roster · `d` digest · `b` bus |
| `f` | find — `a` actions · `s` spaces |
| `p` | project — `f` files · `p` switch · `n` new · `r` rename |
| `w` | window — `v` `s` split · `z` zoom · `x` close · `h j k l` focus |
| `?` | keys |
| `.` | settings |

Three rules make it safe to press:

- **Nothing typed after the leader can reach a program.** A sequence that turns out not to
  exist is dropped, never replayed into the pane — otherwise abandoning `ctrl+space w` would
  type `w` at whatever an agent was doing.
- **`esc` abandons, `backspace` steps back one key.** Backspace off the last key returns to
  the open table rather than leaving it, so a wrong first key costs one press.
- **The status bar shows the trail.** `LEADER w` means one key is held and horde is waiting;
  a mode that quietly stops your keys reaching a pane is the worst thing it could do.

Bare letters work here precisely because they cannot be typing — which is why `[keys]`
refuses a bare printable as a *direct* binding but accepts it after the leader.

Two doors that do not depend on the chord: **`ctrl+b space`** always opens the table, and
**`ctrl+space ctrl+space`** sends a real `ctrl+space` (NUL) to the pane, for the emacs and
readline users whose `set-mark` it is.

### The approval queue

`ctrl+b A` is the other half of that: rather than walking to each blocked agent in turn, it
shows every pending question in one list and lets you answer from there.

```
◍ reviewer   Halo Suite   waiting 12m
  Do you want to make this edit to src/mux.rs?
    1  Yes
    2  Yes, and don't ask again
    3  No, and tell Claude what to do differently

◍ builder    horde        waiting 4m
  Allow npm test to run?
```

| Key | Action |
|---|---|
| `j` / `k` | move between waiting agents |
| `1`–`9`, `y`, `n` | answer the highlighted one |
| `enter` | open its pane instead |
| `esc` | close |

Three things about it are deliberate:

- **Options are shown only for the agent under the cursor.** Six agents' choices at once
  would be thirty lines with no way to tell which digit belongs to which agent, which is the
  mistake that would make answering from here dangerous.
- **A key the agent did not offer does nothing.** A `4` where the agent listed three options
  is ignored rather than forwarded. This window shows you a menu; a keystroke that means
  nothing in it must not mean something in the pane.
- **It stays open after an answer.** Answering one of six is the case it exists for, and the
  agent you answered drops out of the list on the next pass.

The question is read off the agent's screen, which is a heuristic — it handles a numbered
menu and a plain `(y/n)` prompt, box-drawn or not, wrapped or not. When it cannot read one,
the agent is still listed and still says it is waiting, with `enter` to go and look. It will
not guess.

horde only ever sends the key you pressed, to the pane you were pointing at. There is no path
here for typing free text into an agent.

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
copy visible text instead.
