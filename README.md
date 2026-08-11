# horde

An agent-aware terminal multiplexer. A background daemon owns your PTYs, so coding agents
keep working when you close the terminal — and it knows which ones need you.

```
 api-refactor ›  1 agents  2 logs                        ◍1 ⠹1
 horde              ╭ ⠹ builder ──── 2m18s ──╮╭ ◍ reviewer needs you ─╮ bus
─────────────────── │ > applying migration…  ││ Do you want to make    │────────────
 SPACES             │   003_users.sql        ││ this edit to src/mux.rs│ 14:02   ✓
▎● api-refactor   2 │                        ││                        │ builder →
 ○ docs           1 │   3 files changed      ││   ❯ 1. Yes             │  reviewer
───────────────────  ╰────────────────────────╯╰────────────────────────╯ schema is
 AGENTS             ╭ tests ──────────────────────────────────────────────╮ ready
▎⠹ builder    2m18s │ PASS  42   FAIL  0                                  │
 ◍ reviewer blocked │ $                                                   │ 14:03   ⧗
 ○ writer      idle │                                                     │ reviewer →
───────────────────  ╰────────────────────────────────────────────────────╯  builder
 ◍ 1 needs you                                                            LGTM ✓
 ⠹ 1 working
 ctrl+b   3 panes · 3 agents · 1 needs you                    ~/dev/horde
```

Spaces and agents are separate lists. Every agent in the session shows in one flat place
with its own state, whichever space it happens to live in.

## Why

tmux does not know the difference between an agent that is thinking and one that has been
waiting twenty minutes for you to approve a file write. horde does, and it puts that in a
sidebar. It also gives agents an addressable way to talk to each other.

## Documentation

Full docs in [`docs/`](docs/), readable from the terminal with `horde docs <topic>`:

| | |
|---|---|
| **[orchestration](docs/orchestration.md)** | **agents talking to each other — written to be read by an agent** |
| [quick-start](docs/quick-start.md) | install, first session, first agent |
| [concepts](docs/concepts.md) | spaces, tabs, panes, the daemon |
| [agents](docs/agents.md) | detection, states, lifecycle hooks |
| [socket-api](docs/socket-api.md) | the control protocol, every method |
| [configuration](docs/configuration.md) | `config.toml` and the settings page |
| [keys](docs/keys.md) | keybindings, mouse, right-click menus |
| [troubleshooting](docs/troubleshooting.md) | when something looks wrong |

### Point your agents at this

An agent cannot discover any of this on its own. Tell it once — in `CLAUDE.md`, a system
prompt, or the first thing you say to it:

```
You are running inside horde, a terminal multiplexer where agents can talk to each other.
Run `horde docs orchestration` to learn how, then `horde roster` to see who else is here.
```

Every horde pane also carries `HORDE_DOCS` in its environment holding that exact command, so
an agent that inspects its environment will find it.

## Install

```sh
cargo build --release
cp target/release/horde ~/.local/bin/      # or anywhere on PATH
```

## Use

```sh
horde                 # start the daemon if needed, then attach
horde stop            # stop the daemon and everything it owns
horde status          # what the daemon thinks is going on
```

`ctrl+b d` detaches. Your agents keep running. `horde` reattaches.

### Keys

Prefix is `ctrl+b`. `ctrl+b ?` lists everything.

| | |
|---|---|
| `\|` `-` | split right / down (`%` and `"` also work) |
| `h j k l` | move focus — resolved by geometry, not tree position |
| `H J K L` | resize; `ctrl+h/j/k/l` swaps panes |
| `z` `x` | zoom / close pane |
| `c` `n` `p` `1`-`9` | new tab / next / prev / go to |
| `s` `S` | space switcher / new space |
| **`a`** | **jump to the next agent that needs you** |
| `g` | command palette |
| `e` `b` | toggle sidebar / bus drawer |
| `,` `.` | rename the focused pane (also renames the agent) / **settings** |
| `d` `?` | detach / help |

### Mouse

Left-click a pane or a sidebar row to focus it, click a tab to switch, scroll to page back
through scrollback.

**Right-click anything** for a context menu built from what is under the cursor:

| Right-click | You get |
|---|---|
| a pane | split, start an agent, run a command, zoom, rename, copy visible text, send a message (agents only), layout, close |
| a space row | focus, new tab here, rename, new space, close space |
| an agent row | the pane menu for that agent, including **send message** |
| a tab | focus, rename, layout, new tab, close tab |
| anywhere else | new space, new tab, start agent, layout, toggle panels, jump to attention |

Every entry shows its keyboard equivalent, so the menu teaches the keys rather than
replacing them. `›` marks a submenu. Arrow keys or `j`/`k` navigate, `enter` activates,
`esc` steps back out of a submenu and then closes.

### Settings

`ctrl+b .` opens a settings page with categories down the left:

* **Appearance** — theme, sidebar and bus drawer, widths, pane titles, animations
* **Keybindings** — every rebindable action with its current key
* **Agents** — restore, detection depth, force delivery, install Claude hooks
* **Notifications** — in-app, in-app + macOS, or off
* **Terminal** — scrollback, shell
* **About** — versions and every path horde uses

`tab` switches category, `↑`/`↓` moves, `←`/`→` changes a value. Changes apply immediately
and persist. To rebind a key, select the action, press `enter`, then press the key you want:
a bare key becomes a prefix binding, a modified chord becomes a direct one, and a chord
another action already owns is refused rather than double-bound.

Writing goes through `toml_edit`, so comments and formatting in a hand-edited `config.toml`
survive. The page also offers **edit config.toml in $EDITOR** (opens in a new pane) and
**reload from disk**.

## Layouts

```sh
horde layout duo      # solo · duo · trio · dev · quad
```

## Agents

horde detects agents in two tiers, and only one is ever in charge of a given pane.

**Lifecycle hooks (authoritative).** Install them and the agent reports its own state:

```sh
horde integration install claude
```

This merges into `~/.claude/settings.json`, backs the file up first, leaves any other
tool's hooks alone, and is safe to re-run. Restart running Claude sessions afterwards.

**Screen manifests (fallback).** Otherwise horde reads the foreground process and matches
regexes from `agents/*.toml` against the live bottom of the pane buffer. Override any of
them in `~/.config/horde/agents/<name>.toml`.

Hooks are worth installing. Screen detection reads whatever is on screen, and a narrow pane
truncates the very marker it depends on — Claude's `esc to interrupt` sits at the end of a
long status line, so a 22-column pane hides it and the agent looks idle while it works.
Hooks do not care how wide your panes are.

```sh
horde roster                    # names, states, how long, and why
horde agent explain 3           # the snapshot and the exact rule that fired
```

States are `working`, `blocked`, `done`, `idle`, `unknown`. `blocked` is deliberately
strict — only a visible approval or permission prompt counts, never silence. `done` means
*finished while you were not looking*; it clears when you look.

## Agents talking to each other

The daemon routes and records every message, then injects it into the target's PTY.

```sh
horde roster                                    # who is out there
horde send reviewer "schema is ready"           # to one agent
horde broadcast "pausing for a deploy"          # to all of them
horde bus tail -f                               # watch the traffic
```

Inside a pane you do not say who you are — `HORDE_PANE` is in the environment, so the
daemon knows. `ctrl+b b` opens the drawer to watch it live.

**Delivery is gated on the target's state**, which is the part that makes this safe:

| Target | What happens |
|---|---|
| `idle` / `done` | delivered and submitted — it is at its prompt |
| `blocked` | **queued.** It is waiting on a decision; a newline would answer the prompt |
| `working` | queued, flushed when it goes idle |
| `unknown` | queued |

Nothing is silently lost: a held message shows as `⧗ queued` in the drawer and as
`(+1 queued)` in the roster. `--now` overrides the gate, which is exactly as unsafe as it
sounds at a permission prompt.

A pane with no agent gets the text without a submitting newline, so a stray message can
never run as a shell command.

## Agents driving horde

Every subcommand is one socket call, so an agent orchestrates horde with a shell:

```sh
horde spawn --cmd claude --name reviewer --split right
horde pane read reviewer --source detection --lines 40
horde wait reviewer --until done --timeout 300
horde send reviewer "take a look at src/bus.rs"
horde pane list
horde api session.snapshot          # the raw control API
```

## Config

`~/.config/horde/config.toml`. Everything is optional.

```toml
prefix = "ctrl+b"
scrollback = 10000

[theme]
name = "horde"            # horde · tokyo-night · catppuccin · gruvbox · terminal

[theme.custom]
accent = "#7ee2c0"

[ui]
sidebar = true
sidebar_width = 24
bus = false               # ctrl+b b toggles it
pane_titles = true
animate = true

[agents]
restore = true            # resume agents after a daemon restart
detection_lines = 40

[notifications]
delivery = "horde"        # horde · system · off

[keys]
zoom = "prefix+f"
```

`horde keys` lists every action name you can rebind.

## Persistence

`horde stop` and restart, or a crash, restores the *shape*: spaces, tabs, the split tree
and its ratios, names, and working directories. Panes come back as fresh shells — the same
bargain tmux makes.

Agent panes do better. With `restore = true`, an agent whose integration reported a session
id comes back resumed (`claude --resume <id>`). Without a session id it comes back as a
shell, because starting a fresh agent unbidden is presumptuous.

Scrollback contents are **not** written to disk. Terminal output holds secrets, tokens and
command history, and persisting that by default would be the wrong trade.

## How it works

One binary, three roles: `horde` attaches, `horde daemon` serves, `horde <noun> <verb>` is
a control call. Both halves ship together, so there is no protocol skew to manage.

The daemon owns VT emulation, not the client. That is load-bearing — status detection has
to keep working while nothing is attached, so something server-side must always be able to
see the screen. Panes are real PTYs (`portable-pty`) fed into `alacritty_terminal`, and each
pane keeps a mirror of its visible grid. The mirror is the single thing everything else
reads: clients diff against it, detection matches against it, `pane read` returns it.

Two channels share one socket at `~/.config/horde/horde.sock`. Control is
newline-delimited JSON, so it is debuggable with `nc`. On `attach` a connection switches to
length-prefixed `postcard` frames carrying only dirty rows, run-length encoded by style.

Geometry lives in the daemon and nowhere else, so a pane's PTY size and its drawn rectangle
cannot drift apart.

```
src/
  proto.rs         wire types, shared by both halves
  config.rs        TOML config, keymap
  theme.rs         palettes, terminal colour → RGB
  framing.rs       length-prefixed postcard
  daemon/
    mod.rs         engine loop, socket, broadcast
    state.rs       spaces/tabs/panes + all geometry
    layout.rs      BSP tree: split, close, resize, focus, presets
    pane.rs        PTY + emulator + row mirror
    agents.rs      detection tiers, done/seen machine
    manifest.rs    screen-manifest rules
    bus.rs         routing, state-gated injection, queue
    persist.rs     save/restore
    rpc.rs         control methods
  client/
    mod.rs         attach loop, input routing
    input.rs       key/mouse → PTY bytes
    ui/            frame composition, panels, overlays
  cli/             subcommands, hook integration
agents/            bundled detection manifests
```

## The sidebar

Two independent sections:

* **SPACES** — projects only. A dot turns to the attention colour when anything inside needs
  you, so a collapsed row tells you whether it is worth opening.
* **AGENTS** — every agent in the session, wherever it lives, each with its own state and
  either its elapsed time or its status. Ordered stably by space rather than by urgency,
  because rows that jump around under you are worse than rows you have to scan; colour
  already carries the urgency. Agents in other spaces stay listed but render dimmer. Click
  any row to jump to it.

Shell panes are not listed — they are not agents, and you can already see them on screen.

## Notes

* **The daemon outlives your client and survives rebuilds.** After `cargo build`, run
  `horde stop` before reattaching, or you will connect a new client to the old daemon.
  horde warns when it notices this, but stopping is the cure.

* macOS and Linux. No Windows.
* The socket path must stay under ~100 bytes; that is an OS limit on `AF_UNIX`. Set
  `HORDE_SOCKET` if your config directory is deep.
* Not implemented: dragging pane borders to resize (use `H J K L`), text selection in copy
  mode (right-click → copy visible text instead), and OSC 52 clipboard forwarding.
* `cargo test` covers the layout algebra, detection state machine, key encoding, config
  parsing and writing, and cell rendering — the parts where a subtle bug would be hard to
  see.
