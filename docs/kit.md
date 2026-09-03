# The kit

horde is a terminal multiplexer. The **kit** is an optional layer over it: a vault of notes, a
kanban board, a dependency graph, a file viewer and editor, language servers, inline images,
and syntax highlighting.

```sh
cargo install horde                    # the multiplexer
cargo install horde --features full    # and the kit
```

The multiplexer is complete without it. Nothing in the kit is load-bearing for panes, tabs,
spaces, agents, the bus, or worktrees.

## Why a build flag and not a separate crate

The obvious shape for "install this over the top" is a second crate, the way a Neovim
distribution rides Neovim. horde cannot do that yet, and pretending otherwise is what produced
the fork this file exists to explain.

The kit does not sit *on* the multiplexer, it reaches *into* it: its views are horde modes, its
commands are horde keybindings, its notes are pane content, its cards ride the same wire
protocol. A plugin boundary strong enough to carry that is a real piece of engineering — an
ABI, a stable internal API, versioning across it — and none of it exists. Until it does, a
build flag is the honest mechanism: one tree, one protocol number, one lineage, and a core fix
lands in both builds the day it is written.

That last part is not theoretical. The kit lived in a separate repository from 2026-08-14 to
2026-09-03, and in three weeks it missed every terminal-correctness fix the multiplexer
shipped, while the same note-taking feature was implemented twice on the two sides.

## The switch

The feature flag sets a *default*, not a ceiling. What it actually controls is `[kit] enabled`:

```toml
[kit]
enabled = true    # or false
```

Both directions work, and both matter. Someone on a plain build who wants to try the kit
should not have to reinstall to find out whether they like it, and someone on a full build who
only wants the multiplexer should not have to either. The flag decides what is compiled in and
what you get without saying anything; the config key decides what is switched on.

Measured, on macOS arm64, release:

| build | binary |
|---|---|
| `cargo build --release` | 8.2 MiB |
| `cargo build --release --features full` | 14.8 MiB |

Most of the difference is compiled-C tree-sitter grammars, which only the kit's syntax
highlighting uses.

## What happens with the kit off

- **Keys** bound to kit actions are dropped when the config loads, so they do not appear in
  `?` help or on the settings page as though they work.
- **Actions** that reach `run_action` anyway — through a stale config, a rebind — answer with
  a toast naming the switch.
- **The socket API** refuses `vault.*` with a sentence naming the switch, which is what
  `horde note` surfaces to an agent that read about it in a skill file.
- **The start screen** is the terminal. The walkthrough asks about the vault and the dashboard
  is the kit's own front page, so neither is offered.

## Adding to the kit

Put the module beside its neighbours (`daemon/vault.rs`, `client/ui/kanban.rs`, …) and add its
`Action` to `KIT_ACTIONS` in `src/config.rs`. That one list is what the keymap, `run_action`
and the tests all read, so a new surface is gated everywhere by being named once.

If the new thing needs a socket method, gate it beside the `vault.` check at the top of
`rpc::handle`.
