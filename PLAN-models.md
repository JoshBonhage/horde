# Models in horde: OpenRouter, opencode, and switching

**horde does not talk to models, and this plan does not change that.** It spawns programs and
watches their screens. The whole design here follows from taking that seriously: OpenRouter is
configured inside the agent, and horde's contribution is choosing *which* agent starts with
*which* model, noticing when that model stops working, and moving it.

---

## The decision this rests on

There is no HTTP client, no key storage and no model concept anywhere in `src/` — checked, not
assumed. `docs/unattended.md` already states the principle out loud: "there is no HTTP client
here and nowhere to keep a token." Two ways to add models:

| | |
|---|---|
| **In the agent** | opencode owns the provider and the key. horde picks the command and reads the screen. **Chosen.** |
| **In horde** | horde holds the key, knows the catalogue, calls the API. Needs an HTTP client, a secret store and a model registry, and contradicts the line above. **Rejected.** |

The second is not rejected because it is hard. It is rejected because the moment horde holds a
key it acquires a threat model, a refresh story, and a reason to be in the request path of every
agent — and it stops being a multiplexer.

## What already exists

Three things mean this is less new subsystem than it looks.

**Agents already spawn agents.** `horde spawn` is callable from inside a pane, with a fleet cap,
`--worktree`, `--role` and `--board`. The half of the vision about agents begetting agents is
shipped.

**horde already reads the screen for state.** Manifests match text and produce `blocked` /
`working` / `idle`. A rate-limited free model announces itself *in the pane*, as text. That is
the same class of fact manifests already consume.

**horde already types into panes.** The bus injects text into an agent's PTY, gated on the
terminal being in raw mode and the agent being at its prompt. This turns out to be the whole
switching mechanism — see [Switching without losing the session](#phase-3--switching-without-losing-the-session).

**And `--cmd` already carries a model.** This works today, with no code at all:

```sh
horde spawn --cmd "opencode --model openrouter/qwen/qwen3-coder:free" --name builder
```

## What is missing

Three gaps, in dependency order.

1. **Per-pane environment.** `Pane::spawn` (`src/daemon/pane.rs:227`) sets `TERM`, `COLORTERM`,
   `PAGER` and the `HORDE_*` identity vars, and nothing else. There is no `--env` on
   `horde spawn`. So two agents cannot use different keys or base URLs — only different
   command lines.
2. **A model profile.** Without one, the model string is copy-pasted into every `--cmd` and
   changing it means editing every call site.
3. **Somewhere to record which model an agent is on**, so "move to the next one" has a next one.

---

## Phase 0 — make one agent work by hand

No code. The point is to capture the two things every later phase needs: proof the chain works,
and **the literal text opencode prints when a free model is exhausted**, which is what Phase 3's
rule must match. Guessing that string is the most likely way this plan fails.

```sh
curl -fsSL https://opencode.ai/install | bash        # or: brew install anomalyco/tap/opencode
opencode auth login --provider openrouter --method api-key
opencode auth list                                   # credentials land in ~/.local/share/opencode/auth.json
horde spawn --cmd "opencode --model openrouter/qwen/qwen3-coder:free" --name builder
```

Then exhaust it deliberately and screenshot the pane. Also confirm what `horde agent explain`
says about the pane — `agents/opencode.toml` is marked *"Untested locally"* and its `detect`
patterns have never been checked against a running opencode.

## Phase 1 — per-pane environment

`horde spawn --env KEY=VALUE`, repeatable, plus an `[env]` table in `config.toml` applied to
every pane. Touches `Pane::spawn`, the `agent.spawn` RPC (`src/daemon/rpc.rs`), and the CLI.

Two rules worth writing into the code rather than discovering later:

- **Never log the values.** An `--env` carrying an API key must not reach the daemon log, the
  journal, or a `horde status` field. This is the first place horde handles anything secret, even
  in transit, and the reason the plan otherwise keeps secrets out.
- **A pane's env is not restored from `state.json`.** Persisted layout re-spawns commands; if it
  also persisted env, the key would be on disk in horde's own state file. Re-read it from the
  daemon's environment or the config instead.

## Phase 2 — model profiles

```toml
[models.free]
cmd = "opencode --model openrouter/{model}"
order = [
  "qwen/qwen3-coder:free",
  "deepseek/deepseek-chat-v3.1:free",
  "z-ai/glm-4.5-air:free",
]
```

`horde spawn --profile free` starts at the head of `order`. The profile is what a trigger later
advances through, and what an agent spawning another agent can name without knowing any model
strings.

## Phase 3 — switching without losing the session

The obvious design is: detect exhaustion, kill the pane, respawn with the next model. **Do not
do this.** A respawn is a new session — the agent loses its conversation, its plan and whatever
it had read. Switching would cost more than the rate limit did.

horde already has the better mechanism. opencode changes model *in session* through its own
`/models` command, and horde can type into a pane — that is what the bus does all day, with the
raw-mode and prompt-state gating already built. So:

```
manifest rule matches the exhaustion text
        │
        ▼
pane enters a state, or emits an event
        │
        ▼
trigger advances the profile and injects "/models <next>"
        │
        ▼
same session, same context, different model
```

Everything on the left of that chain exists. What is new is the trigger action ("advance this
pane's model") and the per-pane record of where it is in `order`.

Falling back to respawn is the right behaviour only when injection is refused — a pane at a
canonical prompt, or an agent that has stopped reading. The bus already reports both.

## Phase 4 — connecting, from the settings page

The add-on, and the one place where "sign in like herdr" needs care.

The settings page (`src/client/settings.rs`) already has the machinery: a `Field` enum, `rows()`
building `Setting` / `Note` / `ReadOnly` / `Action` rows, and existing actions like *Edit
config.toml* and *Reload from disk*.

**The row must not capture the key.** A text field that takes an API key means horde holds a
secret — the thing this plan is built to avoid. Instead the action **opens a pane running
`opencode auth login --provider openrouter --method api-key`**. The user types the key into
opencode, opencode stores it in its own `auth.json`, and horde never sees it. What horde shows is
*status*, read back from `opencode auth list`:

```
Models
  OpenRouter        connected            [Connect…]
  Active profile    free (3 models)
  Current model     qwen/qwen3-coder:free
```

That is the same UX as a sign-in flow, with none of the custody. It also generalises: any agent
tool with an auth command gets a row, and horde's part stays "spawn a pane, read a status line."

## WSL

Everything here is Linux-side, so it mostly just works — with four specifics.

- **Install opencode inside the distro, and keep the repo in `~`.** Not `/mnt/c`. See
  [wsl](docs/wsl.md); the DrvFs penalty lands hardest on exactly the git-heavy fleet work this
  is for.
- **Use `--method api-key`, not a browser flow.** Browser-based auth from inside WSL depends on
  interop opening a Windows browser and handing a redirect back. The API-key method has no
  redirect and no browser, which makes it the reliable path and the one to document.
- **`auth.json` lives at `~/.local/share/opencode/auth.json`** — the Linux home, inside the
  distro's own filesystem. It survives `wsl --shutdown` and reboots like any other file. It does
  *not* survive `wsl --unregister`.
- **A key exported in `.bashrc` may not reach the daemon.** The daemon is `setsid`'d from
  whichever shell started it and inherits that environment; a daemon started by some other route
  gets a thin one. This is the same class of bug as the `/bin/zsh` fallback. Phase 1's `[env]`
  config table is the fix — the key is read from config by the daemon, not inherited by luck.

## Risks worth stating before building

**Free models are weak at tool use.** Agentic coding depends on reliable tool-calling, and that
is where free open-weight models are furthest behind. The plumbing will work; the quality floor
is the real constraint. Prototype against two models before designing for twenty.

**Rate limits are aggressive enough that switching may be constant.** If a fleet burns through
`order` in minutes, the honest answer is that the free tier does not support the workload — and
horde should say so rather than rotate forever. A "profile exhausted" state that stops and tells
you beats silent thrashing.

**The exhaustion text is provider- and version-specific.** A manifest rule matching it will break
when opencode changes its wording. Phase 0's capture is the input; expect to revisit it, and keep
the rule in `agents/opencode.toml` where a user can override it without rebuilding.

**`agents/opencode.toml` is unverified.** It ships with a comment saying so. Detection may need
real work before any of this is observable in the sidebar.

## Deliberately out of scope

horde holding credentials, calling any API, or proxying model traffic. A model catalogue inside
horde — `order` is a user's list, not a registry horde maintains. Cost tracking and quota
accounting, which need the API responses horde deliberately never sees.
