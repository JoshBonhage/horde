# Quick start

## Install

```sh
cargo build --release
cp target/release/horde ~/.local/bin/      # or anywhere on PATH
```

## First session

```sh
cd ~/some/project
horde
```

That is all. horde starts its own background daemon, creates a space named after the
directory, and drops you into a shell.

`ctrl+b d` detaches. **Your agents keep running.** `horde` reattaches.

## First splits

```
ctrl+b |     split right
ctrl+b -     split down
ctrl+b hjkl  move between panes
ctrl+b z     zoom the focused pane
ctrl+b ?     every key
```

Or right-click anything for a menu.

## First agent

Type `claude` in any pane. Within a fraction of a second the sidebar's AGENTS section picks
it up and starts showing its state.

To start one from outside, named so you can address it:

```sh
horde spawn --cmd claude --name builder
horde roster
```

## Make two agents talk

```sh
horde spawn --cmd claude --name builder
horde spawn --cmd claude --name reviewer
```

Then from inside `builder`'s pane:

```sh
horde send reviewer "read src/bus.rs and tell me if the gating logic is sound"
```

Open the bus drawer with `ctrl+b b` to watch it routed. Full detail in
[orchestration](orchestration.md).

## Come back later

Detach with `ctrl+b d` and go do something else. When you reattach, horde tells you what
changed rather than leaving you to read five panes of scrollback:

```sh
horde digest
```

```
while you were away · 42m

  needs you
    ◍ reviewer         stuck 12m    approval prompt

  board
    ● #4   write the bus tests  [builder]
           → 18 tests added, all passing
    2 open, 1 claimed

  bus · 3 messages
    ✓ ask #7 builder → reviewer: is the gating logic sound?
    ✓ re #7 reviewer → builder: yes — queued messages flush on the next idle pass
```

Reading it advances the window, so the next digest picks up where this one stopped. Use
`--since 2h` to look further back and `--keep` to look without moving the window.

## Worth doing once

```sh
horde integration install claude
```

This installs lifecycle hooks so Claude Code reports its own state instead of horde guessing
from the screen. It merges into `~/.claude/settings.json`, backs the file up first, leaves
other tools' hooks alone, and is safe to re-run. Restart running Claude sessions afterwards.

Why it matters: screen detection reads whatever is on screen, and Claude's
`esc to interrupt` marker sits at the end of a long status line — a narrow pane truncates it
and a working agent looks idle. Hooks do not care how wide your panes are.

## Next

- [concepts](concepts.md) — the model
- [orchestration](orchestration.md) — agents talking to each other
- [keys](keys.md) — the full keymap
