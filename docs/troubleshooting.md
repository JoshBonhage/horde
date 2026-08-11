# Troubleshooting

## An agent's state looks wrong

```sh
horde agent explain <name>
```

This prints what detection is actually looking at and which rule fired. The usual causes:

**The pane is too narrow.** Screen detection reads the pane's visible text. Claude Code puts
`esc to interrupt` at the end of a long status line, so below roughly 60 columns it is
truncated off and a working agent reads as idle. Fix: `horde integration install claude`, or
widen the pane.

**It says it is generating and never stops.** A rule is matching the agent's own transcript
rather than its live status line — "Thinking…" from ten minutes ago is still on screen. The
rule needs `within = N` to scope it to the bottom few lines. See
[agents](agents.md).

**It has the wrong agent's name.** Two manifests are claiming the pane because one has a
generic `detect` pattern that the other agent also shows. `horde agent explain` prints
`chosen` and `manifests_matching`; if more than one is listed, tighten the `detect` list of
whichever does not belong.

**No rule matched.** `reason` will say so, and the state falls back to `idle`. If the agent
updated its UI, the bundled pattern may be stale — copy `agents/<name>.toml` to
`~/.config/horde/agents/<name>.toml` and fix the pattern. `agent explain` shows you the
snapshot to write it against.

**Hooks were installed but nothing changed.** Hooks only take effect in Claude sessions
started *after* installing. Restart them.

## Nothing I do in the UI has any effect

Almost certainly a stale daemon. It outlives every client and survives rebuilds, so a new
client can attach to a daemon built from older code.

```sh
horde stop && horde
```

horde warns on a version mismatch, but stopping is the cure. `horde status` shows the running
daemon's version.

## A message never arrived

```sh
horde bus tail --limit 20
```

Every routed message is there with its delivery state.

- `⧗ queued` — the target was not at its prompt. This is normal and it will flush
  automatically. If it stays queued, the target is probably `blocked` and needs a human.
- `✕ dropped` — the target pane went away.
- `✓ delivered` — it was typed into the pane. If the agent did not act on it, the message
  may have read as chat rather than an instruction. See
  [orchestration §3](orchestration.md).

## `horde send` says there is no such agent

```sh
horde roster
```

Names come from detection, so an agent that has not been detected yet has no name. A freshly
spawned `claude` takes a moment to boot. Either wait:

```sh
horde wait reviewer --until idle --timeout 60
```

or send anyway and let the queue hold it.

## The socket path is too long

```
socket path is too long for the OS (164 bytes, limit ~100)
```

`AF_UNIX` caps paths at about 104 bytes. If your config directory is deep:

```sh
export HORDE_SOCKET=/tmp/horde.sock
```

## Panes did not come back after a restart

Restarting restores the *shape* — spaces, tabs, the split tree and its ratios, names, working
directories. Panes come back as fresh shells, the same bargain tmux makes. Scrollback is
deliberately not persisted: terminal output holds secrets, tokens, and command history.

Agent panes do better if `agents.restore = true` and the agent reported a session id.

## Where things live

```sh
horde status                      # versions, counts, and every path
```

| Path | What |
|---|---|
| `~/.config/horde/config.toml` | configuration |
| `~/.config/horde/horde.sock` | the control socket |
| `~/.config/horde/state.json` | saved session shape |
| `~/.config/horde/bus.jsonl` | every routed message |
| `~/.config/horde/horde.log` | daemon log — start here when the daemon misbehaves |
| `~/.config/horde/agents/` | your detection manifest overrides |
