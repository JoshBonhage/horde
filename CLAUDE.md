# horde

**An agent-aware terminal multiplexer.** A daemon owns every PTY; a thin client attaches to
it. That is the whole product, and it is complete on its own.

## What belongs here

The multiplexer and nothing else: PTY ownership, VT emulation, the wire protocol, layout,
panes, tabs, spaces, the sidebar, agent detection, the bus, worktrees, the socket API,
rendering, input, config, themes.

## What does NOT belong here

Two sibling repos exist. **Do not read them, port from them, or install their binaries while
working in this one** unless the task explicitly says to.

`~/Documents/dev/horde-full` was a fork of this repo and was **merged back on 2026-09-03**
(branch `merge/horde-full`). Its features are now the **kit**, a cargo feature of this tree —
see [docs/kit.md](docs/kit.md). Do not work in that directory; it is history.

`~/Documents/dev/horde-desktop` is still separate and stays that way: an Electron task manager
that vendors its own `crates/horde` and talks to the daemon over the socket. Do not port
between it and here.

## Traps

**Josh runs the plain build** (`cargo build --release`, no features). Install `--features full`
only when he asks to test the kit.

**`cargo test` writes into the real `~/.config/horde/horde.log`.** `log_line` goes through
`config_dir()` and nothing sets `HORDE_CONFIG_DIR` for tests. The board, bus and journal are
properly isolated to `temp_dir()`; only the log leaks. Set `HORDE_CONFIG_DIR` to a temp dir
when testing against a live session.

**Building and testing against a live daemon can end with that daemon handed over to a freshly
built binary.** After a test run that matters, check `horde status` and
`shasum ~/.local/bin/horde`.

**Before installing**, run `horde status` and compare its `protocol` line to
`PROTOCOL_VERSION` in `src/proto.rs`. Installing also needs a re-sign, or every launch exits
137 with no output:

```sh
cargo build --release
rm -f ~/.local/bin/horde && cp target/release/horde ~/.local/bin/horde
codesign --force --sign - ~/.local/bin/horde
```

## Prior art

`herdr` (`~/Documents/dev/herdr`) is the project horde's premise is credited to, and it is a
legitimate reference for *how* to do something — its render path in particular. Read it when
comparing approaches; do not copy code.
