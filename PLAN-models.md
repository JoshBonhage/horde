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

## Phase 1 — per-pane environment — **shipped**

Shipped as the `[env]` table in `config.toml`, applied to every pane. Per-spawn `--env` was
**deliberately deferred**: it is only needed when two agents want *different* keys, and varying
the model — the actual goal — is already expressible through the command line. The table solves
key delivery, which is the part that blocks everything else.

Spawning now also names the program it could not start. `failed to spawn command` alone sent
someone hunting for a config problem when opencode simply was not installed.

Two rules worth writing into the code rather than discovering later:

- **Never log the values.** An `--env` carrying an API key must not reach the daemon log, the
  journal, or a `horde status` field. This is the first place horde handles anything secret, even
  in transit, and the reason the plan otherwise keeps secrets out.
- **A pane's env is not restored from `state.json`.** Persisted layout re-spawns commands; if it
  also persisted env, the key would be on disk in horde's own state file. Re-read it from the
  daemon's environment or the config instead.

## Phase 2 — model profiles — **shipped**

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
strings. `ModelProfile::command(index)` returns `None` past the end rather than wrapping, so
Phase 3 inherits "the list is spent" as a state it must handle rather than a case it can ignore.

## Phase 3 — switching without losing the session — **shipped**

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

## Phase 5 — succession: when the agent itself runs out — **the warning half is shipped**

Phase 3 moves a *model* inside a running agent. This is the harder case: the agent is finished —
its plan is out of usage, not out of model — and something else has to carry on. "Claude runs out
overnight, a free opencode picks it up, and when that one dies the next takes over."

### What is actually detectable

**Shipped:** `[handover]` watches for a warning on an agent's pane and tells that agent to hand
over while it can still act — it writes its own note and spawns its own successor. What is not
built is the case where no warning ever appeared and the agent died silently; that still needs
horde to compose the brief from what it watched.

**"Out" is detectable. "Nearly out" mostly is not.** A usage limit announces itself in the pane
as text, which is exactly what manifests already read. Remaining budget does not appear anywhere
horde can see — there is no API here by design — so any "switch at 90%" design needs a number
horde cannot obtain. Two honest paths instead:

- **Cooperative.** The agent notices and says so: `horde broadcast "hit my usage limit"`. Better
  quality, because the agent knows what it was in the middle of. Already added to the skill.
- **Observational.** A manifest rule matches the limit message. The safety net for an agent that
  stops mid-sentence and never gets to speak.

Build both. The cooperative path carries the good handover; the observational one guarantees
*something* happens.

### Who writes the handover, and when

The first design here had horde detect the death and compose the brief. A better shape is for
the **dying agent to spawn its own successor and hand over directly**, while it still can:

```bash
horde spawn --profile fallback --name builder-2 --worktree \
  --brief "You are taking over from me. The parser port is half done: src/parse.rs compiles,
           tests not written. I ran out of usage. Read the diff before changing anything."
```

That is better for the reason that matters most — **the agent knows what it was doing, and horde
only knows what it could see.** A self-written brief beats any reconstruction from diffs and
scrollback.

`--brief` is built (Phase 4a below). It exists because a freshly spawned pane has no agent for a
second or two, so a brief sent immediately would be typed into a booting TUI without a newline —
which is why `--task` went through the board. The brief waits as an orphan and is re-homed by
name the moment detection finds the agent, reusing the mechanism that already recovers queued
messages across a restart.

**The two paths are complementary, not alternatives:**

| | Written by | Quality | Works when |
|---|---|---|---|
| Agent-driven | the dying agent | high — it knows the plan | it noticed in time |
| horde-driven | horde, from what it watched | adequate | the agent died mid-sentence |

Build the agent-driven path first; it needs no detection at all, only an agent that knows to do
it. The horde-driven one is the net.

### What horde can reconstruct when nobody handed over

Spawning a successor is trivial — `agent.spawn` already does it, with the worktree, role, space
and name of the pane it replaces. What is hard is that a fresh agent knows nothing.

The instinct is to have the dying agent write a handoff note. **It cannot** — it is out of
tokens; that is the premise.

But horde has been watching the whole time, and can compose the brief itself from things it
already holds:

| Source | What it contributes | Already exists |
|---|---|---|
| the spawn's `--task`, or the board entry | what this agent was *for* | yes |
| `repo.rs` facts on its worktree | `git status`, diff stat — what changed | yes |
| the pane's scrollback tail | what it was doing when it stopped | yes |
| the bus log to and from it | what it was told and what it promised | yes |
| the journal | when it started, what states it passed through | yes |

That is a better handover than most humans write, and it costs the dying agent nothing. It is
also the argument for doing this in horde rather than in a shell script: **the multiplexer is the
only thing in the system that watched.**

### The chain

A chain is an ordered list of what to try, and `[models.<name>]` is already an ordered list.

```toml
[models.fallback]
cmd = "opencode --model openrouter/{model}"
order = ["cohere/north-mini-code:free", "openai/gpt-oss-20b:free"]
```

```sh
horde spawn --cmd claude --name builder --succeed-with fallback
```

Exhaustion advances one step; a spent chain stops and notifies rather than wrapping — same rule
as Phase 2, for the same reason.

### Four ways this goes wrong

**Provenance disappears.** You go to bed with Claude on a task and wake to work done by a 20B
free model, with nothing saying so. This is the most likely real harm and the easiest to fix:
the succession must be journalled, visible in the roster, and in the digest headline —
*"builder handed over to opencode/north-mini at 03:12"*. horde already has all three surfaces.

**A weaker successor undoes good work.** It inherits a half-finished change it did not write, at
full permission. Mitigation is to brief it to *read before continuing*, and possibly to commit
the predecessor's work first so there is something to go back to.

**Infinite succession.** A chain whose members all die immediately spawns forever. It must count
against `max_spawned`, and a member that dies within some floor of starting should end the chain
rather than advance it — that is a broken configuration, not a run of bad luck.

**Cost inversion.** A chain that falls back to a *paid* model spends money unattended, which is
the opposite of the intent. Falling back should never move to a more expensive model than the one
that failed, and horde cannot know prices — so the safe rule is that a chain is opt-in per name
and the human owns its ordering.

### Why this is worth building

Not because free models are good. Because the alternative to a weaker agent finishing the job is
usually **nothing happening for eight hours**, and horde's entire premise is that the hours
nobody is watching should not be dead time. A rate limit at 2am currently costs a night. It
should cost a downgrade.

## Phase 4a — `--brief` — **shipped**

`horde spawn --brief "..."` gives a new agent its first instruction with no board involved. The
message is held until an agent answering to that name exists and reaches its prompt.

This closes a gap that `agents.board = false` opened: with the board closed, `--task` is refused,
and there was no other way to tell a spawned agent what it was for. It is also the primitive
succession needs.

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
