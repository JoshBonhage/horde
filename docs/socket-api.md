# Socket API

The control protocol. Every `horde <noun> <verb>` command is one call to it, so anything that
can write a line to a Unix socket can drive horde — including an agent that would rather
speak the protocol than shell out.

## Transport

Socket at `~/.config/horde/horde.sock`, overridable with `HORDE_SOCKET`.

Newline-delimited JSON, one request per line, one response per line:

```json
{"id":"1","method":"ping","params":{}}
{"id":"1","result":{"type":"pong","protocol":1}}
```

Errors come back as:

```json
{"id":"1","error":{"code":"not_found","message":"no such pane"}}
```

Codes are `bad_request`, `not_found`, and `failed`.

Debug it by hand:

```sh
echo '{"id":"1","method":"server.status","params":{}}' | nc -U ~/.config/horde/horde.sock
```

Or go through the CLI, which handles framing for you:

```sh
horde api server.status
horde api pane.read --params '{"pane":3,"source":"detection","lines":40}'
```

A connection that sends `{"method":"attach"}` stops speaking JSON and switches to the binary
render stream. Do not send `attach` unless you are writing a client.

## Pane targets

Anywhere a method takes `pane`, it accepts either a number (`3`) or a string, which is
resolved in this order: agent name, pane name, pane id, then `space:pane` /
`space:tab:pane` coordinates with 1-based indices.

Omit `pane` and most methods fall back to `HORDE_PANE` from your environment, then to the
focused pane. That is why an agent never has to say who it is.

## Methods

### Server

| Method | Params | Returns |
|---|---|---|
| `ping` | — | `{type:"pong", protocol}` |
| `server.status` | — | version, protocol, counts, paths, theme, loaded manifests |
| `server.reload_config` | — | re-reads `config.toml`, applies it, returns warnings |
| `server.stop` | — | stops the daemon and every pane it owns |

### Session

| Method | Params | Returns |
|---|---|---|
| `session.snapshot` | — | the whole session: spaces, tabs, panes, agents, focus, geometry |

`session.snapshot` is the one call that tells you everything. It is what the client draws
from.

### Spaces

| Method | Params |
|---|---|
| `space.list` | — |
| `space.create` | `name?`, `cwd?` |
| `space.focus` | `name` |
| `space.close` | `name` — kills every process in it |
| `space.rename` | `name`, `to` — answers with the name it actually got, since a clash is uniquified |
| `space.accent` | `name?`/`space?`, `slot?` — omit the slot to step to the next colour |
| `space.collapse` | `name?`/`space?`, `collapsed?` — omit to toggle |

### Tabs

| Method | Params |
|---|---|
| `tab.list` | — |
| `tab.create` | `name?` |
| `tab.close` | — closes the focused tab |
| `tab.rename` | `tab?`, `name` |

### Roles

| Method | Params | Notes |
|---|---|---|
| `role.list` | — | every role in use and how many panes wear it, declared or not |

A role is a job you give a pane — `reviewer`, `builder`, `docs` — and is distinct from both
the pane's name (how you address it) and the agent's kind (which program was detected). Only
the role recurs across projects, which is what makes `role.list` able to answer "who is
reviewing, everywhere" in one call.

Names are normalised on the way in: trimmed, lowercased, spaces and underscores folded to
`-`, capped at 16 characters. So `Code Reviewer` is stored and returned as `code-reviewer`.
Set a role that is not declared in `config.toml` and it still works — declaring one only
chooses how it looks.

### Panes

| Method | Params | Notes |
|---|---|---|
| `pane.list` | — | includes geometry, agent state, scroll offset |
| `pane.current` | — | your own pane id |
| `pane.split` | `pane?`, `direction`, `cmd?`, `name?` | direction: `right`/`left`/`up`/`down` |
| `pane.close` | `pane?` | |
| `pane.focus` | `pane` | also clears a `done` badge |
| `pane.rename` | `pane`, `name` | empty name clears it |
| `pane.role` | `pane?`, `role` | what the pane is *for*; empty clears it. Answers with the **normalised** name |
| `pane.pin` | `pane?`, `pinned?` | hold it at the top of the sidebar; omit to toggle |
| `pane.read` | `pane?`, `lines?`, `source?` | source: `visible`/`recent`/`detection` |
| `pane.send_text` | `pane?`, `text`, `submit?` | raw write; **bypasses the bus and its state gate** |
| `pane.report_agent` | `pane?`, `state`, `session?` | what lifecycle hooks call |
| `pane.scroll` | `pane?`, `lines` | `0` returns to the bottom |

`pane.send_text` writes directly with no routing, no record, and no state gate. Use
`bus.send` for agent-to-agent messages — that is what the gate and the log exist for.

### Layout

| Method | Params |
|---|---|
| `layout.apply` | `preset` — `solo`, `duo`, `trio`, `dev`, `quad` |

Spawns or closes panes to match the preset's pane count.

### Agents

| Method | Params | Notes |
|---|---|---|
| `agent.list` (alias `roster`) | — | every agent, ordered as the sidebar shows them |
| `agent.explain` | `pane?` | the detection snapshot and which rule fired |
| `agent.spawn` | `cmd?`, `name?`, `split?`, `role?`, `worktree?`, `board?`, `task?`, `from?` | `cmd` defaults to `claude`. `worktree` is `true` (name it after the agent) or a name. `from` is the calling pane, and a spawn from one counts against `agents.max_fleet` |
| `agent.wait` | — | **not implemented server-side.** Waiting would stall the single-threaded engine, so the CLI's `horde wait` polls `agent.list` instead. Calling it returns an error saying so. |

### Bus

| Method | Params | Notes |
|---|---|---|
| `bus.send` | `to`, `body`, `from?`, `force?` | returns the `Message`, including its `delivery` |
| `bus.broadcast` | `body`, `from?`, `space?` | every agent but the sender |
| `bus.tail` | `limit?` | recent messages |

`from` is a pane target. Omit it and the daemon uses `HORDE_PANE`.

`delivery` is `delivered`, `queued`, or `dropped`. Queued is not a failure — see
[orchestration §4](orchestration.md).

### Tasks

Every task belongs to a project, taken from the calling pane's space unless `space` says
otherwise. `task.claim` without an id only ever returns work from the caller's own project.

| Method | Params | Notes |
|---|---|---|
| `task.add` | `text`, `from?`, `space?` | returns the new `Task`. `space` defaults to the caller's |
| `task.work` | `from?`, `on?` | enlist this pane for board work. Nothing is offered to a pane that has not |
| `task.claim` | `from?`, `task?` | omit `task` to take the oldest open one **in this project** |
| `task.clear` | `from?`, `everywhere?`, `claimed?` | drop this project's open tasks |
| `task.done` | `from?`, `task?`, `result?` | omit `task` to finish the one you hold |
| `task.release` | `task`, `drop?` | back on the board, or abandoned |
| `task.list` | `from?`, `everywhere?` | this project's tasks, including finished ones |

`task.claim` returns `null` — not an error — when the board is empty, so a worker loop can
tell "no work" from "broken". Claiming a specific task that someone else already holds *is*
an error: the claim is a compare-and-set, and losing the race has to be visible.

A task whose owner is no longer a live pane is returned to the board automatically.

### Digest

| Method | Params | Notes |
|---|---|---|
| `digest` | `since?`, `keep?` | `since` is a lookback in **seconds** |

Returns everything that happened in the window: agents that need a human, agents that
finished, the board's closures, routed messages, panes that exited, and warnings.

Reading advances the watermark so the next call starts where this one ended. Pass
`keep: true` to look without advancing — that is what the client does to build its on-attach
toast, so that `horde digest` afterwards still has the detail.

With no `since` and no previous read, the window starts at daemon start rather than at the
beginning of the logs; the returned `fresh` flag says when that happened.

### Commands

| Method | Params |
|---|---|
| `command` | `name` — runs any keybinding action by name |

Names: `split-right`, `split-down`, `close-pane`, `zoom`, `focus-left/right/up/down`,
`new-tab`, `next-tab`, `prev-tab`, `close-tab`, `new-space`, `next-space`, `prev-space`,
`toggle-sidebar`, `toggle-bus`, `jump-attention`.

This is the same table the command palette uses, so anything you can do from `ctrl+b g` you
can do from a script.

## Example: a roster-driven decision

```sh
horde api agent.list | python3 -c '
import json, sys, subprocess
for a in json.load(sys.stdin):
    if a["state"] == "blocked":
        print(f"{a[\"name\"]} needs a human ({a[\"reason\"]})")
    elif a["state"] in ("idle", "done"):
        subprocess.run(["horde", "send", a["name"], "status check: what are you working on?"])
'
```
