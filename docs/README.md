# horde documentation

An agent-aware terminal multiplexer. A background daemon owns your PTYs, so coding agents
keep working when you close the terminal — and it knows which ones need you.

Read any page from the terminal with `horde docs <topic>`.

## Start here

| Page | What it covers |
|---|---|
| [quick-start](quick-start.md) | install, first session, first agent |
| [concepts](concepts.md) | spaces, tabs, panes, the daemon, why the split matters |
| [keys](keys.md) | every keybinding, the mouse, right-click menus |
| [kanban](kanban.md) | your own board — not the agents' one |

## Agents

| Page | What it covers |
|---|---|
| **[orchestration](orchestration.md)** | **agent-to-agent messaging — the main event. Written to be read by an agent.** |
| [agents](agents.md) | detection, the six states, lifecycle hooks vs screen manifests |
| [worktrees](worktrees.md) | one git worktree per agent, so a fleet in one repo cannot overwrite itself |
| [socket-api](socket-api.md) | the control protocol, every method, for scripting and for agents |

## Reference

| Page | What it covers |
|---|---|
| [configuration](configuration.md) | `config.toml`, themes, the settings page |
| [unattended](unattended.md) | triggers, and how horde reaches you when nothing is attached |
| [wsl](wsl.md) | running horde on Windows, under WSL2 |
| [troubleshooting](troubleshooting.md) | when something looks wrong |

## Point your agents at this

An agent has no way to discover any of this on its own. Tell it once:

```
You are running inside horde, a terminal multiplexer that lets agents talk to each other.
Run `horde docs orchestration` to learn how, then `horde roster` to see who else is here.
```

Every horde pane also has `HORDE_DOCS` in its environment holding that command.
