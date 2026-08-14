# horde on Windows, via WSL2

**The Windows story is WSL2. Not a port — a Linux binary, built and run inside the distro,
with the seams where WSL is not quite Linux handled honestly and the rest documented.**

The README currently says "macOS and Linux. No Windows." The second sentence is the one this
plan changes. The first sentence is the problem: horde has never been run on Linux either, and
WSL2 *is* Linux. Most of the work below is not WSL work at all.

---

## What "WSL only" means

**In scope.** `cargo build` inside an Ubuntu-on-WSL2 distro produces a working horde. The
daemon survives closing Windows Terminal. Clipboard, notifications and the filesystem behave,
or fail with an error that says what to do. A `docs/wsl.md` that a Windows user can follow
without already knowing WSL's quirks.

**Not in scope.** No ConPTY, no named pipes, no `#[cfg(windows)]`, no `horde.exe`. No client on
Windows talking to a daemon in WSL. No WSLg. The one deliberate consequence: the terminal you
type into is a Linux terminal, and everything horde spawns is a Linux process.

## The two halves

| | |
|---|---|
| **Half A — Linux correctness** | Five macOS-only assumptions in the code and no CI to catch a sixth. Bigger than it looks, and pays for itself on plain Linux too. |
| **Half B — WSL seams** | Instance lifetime, clipboard, notifications, DrvFs. One of these is load-bearing for horde's core promise; the rest are papercuts. |

Half A is most of the engineering. Half B is most of the risk.

---

## Half A — what breaks on Linux

There are **no `cfg(unix)` / `cfg(target_os)` gates anywhere in `src/`**, and no `.github/`.
525 tests, never run on anything but a Mac until now.

### What Linux actually said

Run in a `rust:1-bookworm` container (rustc 1.97.1, Linux 6.8 aarch64), source mounted, target
dir on a separate volume:

- **Compiles clean. Zero errors, zero warnings.** With no `cfg` gates anywhere, that was not a
  given: the PTY layer, the `SCM_RIGHTS` handoff, the ioctls and the tokio unix-socket transport
  all built unmodified.
- **`$SHELL` unset: 473 passed, 52 failed — every failure the same line**, `Unable to spawn
  /bin/zsh ... (ENOENT)`, via `src/daemon/mod.rs:1334`.
- **`SHELL=/bin/bash`: 524 passed, 1 failed.** The straggler is a test fixture with a literal
  `"cmd": "zsh"` in a serialised state document (`src/daemon/persist.rs:602`) that `restore`
  then actually spawns.

So the whole Linux delta in the suite is item 3 below: one line of product code and one test
fixture. Better than this plan originally assumed.

**But the green suite proves less than it looks.** Nothing in it calls `copy_to_clipboard`,
`notify_system` or `deliver_system` — those three have production call sites and no test
coverage at all. Items 1 and 2 are exactly as broken on Linux as they were before the run; the
suite simply never looks at them. That is itself an argument for Phase 1: CI that goes green
while the clipboard is dead is CI that teaches you the wrong thing.

**1. Clipboard is `pbcopy`, with no fallback.** `copy_to_clipboard`
(`src/client/mod.rs:999`) shells out to `pbcopy` and nothing else. On Linux the spawn fails and
the user gets `copy failed: No such file or directory`. The README already lists OSC 52
forwarding under "not implemented" — that is the fix, and it fixes SSH at the same time.

**2. Notifications are `osascript`, in two places.** `notify_system`
(`src/client/mod.rs:253`, attached toast) and `deliver_system` (`src/daemon/notify.rs:174`,
detached alert). The client's ignores the error; the daemon's logs `notification could not
start` once per alert. `Notify::System` (`src/config.rs:161`) is documented as "toast plus a
macOS notification" — on Linux the setting is inert and says so nowhere the user will look.

**3. `default_shell()` falls back to `/bin/zsh`** (`src/daemon/pane.rs:851`) — **confirmed, and
it is the entire Linux delta.** `$SHELL` is set in any real session, but the case where the
fallback matters is a daemon started with a thin environment, which is exactly where it breaks.
Ubuntu-on-WSL ships no zsh. `/bin/sh` is the correct fallback off macOS. Fix the fixture at
`src/daemon/persist.rs:602` at the same time.

**4. `ps -o comm=` means two different things — confirmed.** `process_name`
(`src/daemon/agents.rs:525`) reads the foreground process to identify an agent. On Linux
`ps -o comm= -p 1` printed a bare `bash`; macOS prints the full executable path. Linux also
truncates to 15 characters. `process_base()` copes with the path form, so this is latent rather
than broken — the longest name in `agents/` is `cursor-agent` at 12 — but a manifest for anything
longer would silently never match, and no test would notice. Reading `/proc/<pid>/comm` on Linux
is both correct and cheaper than a fork+exec on every detect tick.

**5. Reinstalling fails differently.** `docs/quick-start.md:8` and the troubleshooting entry at
`docs/troubleshooting.md:154` both explain the macOS symptom — a code-signature mismatch, then
SIGKILL on exec. Linux gives `ETXTBSY` / "Text file busy" at copy time instead. `rm -f` first is
the fix on both; only the macOS half is written down.

**Two things that are already fine, verified by reading:**

- The PTY reader handles Linux's EOF correctly by accident. Linux returns `EIO` where macOS
  returns a zero-length read once the last slave closes; `src/daemon/pty.rs`'s `Err(_) => break`
  lands in the same place as `Ok(0) => break`. **Now pinned by a test**, because the accident is
  one plausible tidy-up away from breaking: narrowing that arm to specific error kinds would
  leave every exited pane on Linux spinning in a reader that never finishes.
- The socket path budget. `config_dir()` is already XDG-correct, and the ~100-byte assertion in
  `config.rs` is macOS's 104-byte `sun_path`; Linux gives 108. No change.

---

## Half B — where WSL is not Linux

### 1. Daemon lifetime — the one that could sink this

horde's whole pitch is *close the terminal, the agents keep working*. `ensure_daemon`
(`src/main.rs:95`) earns that with `setsid`, which is correct on Linux. WSL2 adds a second
kill switch above it: the distro instance is torn down once it looks idle, and the VM goes
with it.

What the evidence says:

- A process backgrounded from an **interactive** session does keep the instance alive —
  demonstrated with `nohup sleep 10000 &` in
  [microsoft/WSL#8661](https://github.com/microsoft/WSL/issues/8661). horde's daemon is in
  that category, which is the answer we want.
- A process started from `[boot] command` does **not**; the instance terminates about 15s
  later. Same issue.
- `vmIdleTimeout` (default 60000ms, `.wslconfig`, Windows 11 only) is widely reported not to
  do what its name suggests —
  [microsoft/WSL#9968](https://github.com/microsoft/WSL/issues/9968) is a user setting it to
  seven days and still losing the instance. Do not build on it.
- The "8 second rule" in
  [Microsoft's own docs](https://learn.microsoft.com/en-us/windows/wsl/wsl-config) is about
  config reload, not process lifetime, and is routinely confused with it. Do not cite it.

So the expected answer is "it works", the documented answer is a mess, and the only acceptable
basis for a README claim is a filled-in test matrix (Phase 0). Note also that VM teardown
delivers no SIGTERM, so the handler at `src/daemon/mod.rs:376` never runs — recovery leans
entirely on the debounced `persist::save` (1s after any change, `SAVE_DELAY`), which is a good
place to be. Panes' child processes die either way; that is true of a reboot on any OS and is a
docs problem, not a code one.

### 2. Clipboard

OSC 52 is the right answer and Windows Terminal supports the copy direction —
[microsoft/terminal#5823](https://github.com/microsoft/terminal/commit/b24579d2b04bcbf177c513e4d1885d12511b3aee),
later extended to conhost, with a setting added in Preview 1.23 to turn it off. Query is
deliberately unsupported everywhere, which horde does not need. Fallback chain when OSC 52 is
disabled or the terminal is something else: `clip.exe` under WSL, `wl-copy` / `xclip` on plain
Linux, `pbcopy` on macOS.

### 3. Notifications

There is no native path. The candidates are `notify-send` (needs a distro package and a
notification daemon; WSLg does not reliably bridge toasts), `powershell.exe` with BurntToast
(needs a PowerShell module installed on the Windows side), and `wsl-notify-send.exe` (a
third-party exe).

**Recommendation: build none of them.** Make `Notify::System` resolve a strategy at startup
instead of hardcoding `osascript`, ship `notify-send` as the Linux strategy, and let WSL fall
through to the `notify_command` hook with a documented PowerShell one-liner. That keeps horde's
"no HTTP client, no secret store, the command hook is the reach" posture intact and adds no
Windows-side dependency.

### 4. DrvFs — `/mnt/c` is not a filesystem horde can use

Two distinct failures:

- **Config on `/mnt/c` breaks the socket.** A unix socket cannot be bound on DrvFs. If
  `HORDE_SOCKET` or `XDG_CONFIG_HOME` points under `/mnt/`, `UnixListener::bind`
  (`src/daemon/mod.rs:333`) fails with an errno that explains nothing. One targeted check with a
  real message. *(Confirm the exact errno in Phase 0 before writing the message.)*
- **Repos on `/mnt/c` are slow enough to change behaviour.** Git over DrvFs is an order of
  magnitude slower, and `--worktree` multiplies it — a fleet spinning up worktrees is the worst
  case horde has. A warning when a space's cwd is under `/mnt/` is cheap and will save someone a
  day of thinking horde is slow.

### 5. Everything else, briefly

- **Interop is assumed.** `clip.exe` and `powershell.exe` are on `$PATH` only while
  `[interop] enabled` and `appendWindowsPath` are true — both default, both switched off by
  people who care about `$PATH` hygiene. Every fallback must degrade quietly.
- **Glyphs.** The sidebar's state marks and box drawing need a font that has them. Cascadia Mono
  does not; Cascadia Code NF does. One line in the docs.
- **Timezone.** `localtime_r` at `src/daemon/triggers.rs:707` drives `--at 09:00`. WSL syncs the
  Windows timezone by default (`[time] useWindowsTimezone`), but a distro left on UTC will fire
  every scheduled trigger at the wrong hour and look like a horde bug. Worth surfacing the
  resolved local time in `horde status`.
- **Terminal capability is fine.** Truecolor, mouse, bracketed paste and the alternate screen —
  everything `src/client/mod.rs:328` enables — all work in Windows Terminal.

---

## Phases

**Phase 0 — prove it — outstanding, and blocked on hardware.** No code. A Windows 11 machine,
WSL2, and the matrix below filled in. If the daemon does not survive closing every terminal
window, everything after this changes shape and the honest answer to a Windows user is different.
Also settles the DrvFs errno.

> **Do not try this in a VMware Fusion VM on an Apple Silicon Mac.** WSL2 is itself a Hyper-V
> VM, so it needs nested virtualization. Apple exposes that on M3 and later, and Parallels uses
> it — Fusion does not, as of 13.6.4 (July 2025), and
> [Broadcom lists WSL2 among the features that will not function](https://knowledge.broadcom.com/external/article/315602).
> The symptom is a loop: installing a distro reports that virtualization is not enabled and
> suggests `wsl --install --no-distribution`, which reports success and changes nothing. The
> confirming tell is `systeminfo` reporting **`VM Monitor Mode Extensions: No`**. An hour was
> spent finding this out; that hour is what this note exists to save.
>
> Workable routes, cheapest first: a cloud Windows VM on an instance type permitting nested
> virtualization — also x86_64, which matches real WSL users better than an ARM VM — covers every
> row except sleep and hibernate, which are laptop-hardware behaviours. A physical Windows PC
> covers all of them. Parallels on an M3+ Mac works but costs a subscription.

**Phase 1 — Linux CI — shipped.** `.github/workflows/ci.yml`, four jobs, no third-party actions
beyond `actions/checkout`:

- **test × 3** — linux, macos, and *linux with `$SHELL` empty*. That third one is the whole point
  of the file: the zsh bug was invisible from any machine a human uses, because `$SHELL` is
  always set on one. All three run `--locked`, so lockfile drift fails here rather than passing
  quietly.
- **clippy** — deliberately not `-D warnings`. There is a pre-existing style backlog, and a gate
  that is red on arrival gets ignored rather than fixed. It still fails on the deny-by-default
  correctness lints. Tighten once the backlog is cleared.
- **msrv** — and this job earned its place immediately: `rust-version` said 1.85 and the locked
  tree has needed **1.88** for some time (ratatui 0.30, darling, time). Corrected in `Cargo.toml`
  and the README badge, with 1.88 verified as the real floor rather than assumed.

Reproduced locally any time with:

```sh
docker run --rm -v "$PWD":/work -v horde-linux-target:/target \
  -w /work -e CARGO_TARGET_DIR=/target rust:1-bookworm cargo test --all
```

Add a WSL job if the runners make it practical; if not, Phase 0's matrix is re-run by hand at
release.

**Phase 2 — the platform seams — shipped.** All four now live in `src/platform.rs`, which is the
answer to "what does horde assume about the machine":

- **Clipboard.** A resolved local program first — `pbcopy`, `clip.exe` under WSL, `wl-copy`,
  `xclip`, `xsel`, each gated on evidence that it is the right one — falling through to OSC 52
  when none is present or the one that is present fails. OSC 52 is second rather than first
  because it is write-only: a terminal that ignores the sequence looks exactly like one that
  obeyed it. Capped at 64 KiB, since a truncated sequence spills its payload onto the screen
  rather than erroring. Base64 written out rather than pulled in.
- **Notifications.** `system_notify()` returns a prepared command — `osascript` on macOS,
  `notify-send` where present — or `None`, and the daemon logs the reason once. No WSL case, by
  the reasoning above; the documented route is `notify_command`.
- **Shell.** `/bin/sh` off macOS, empty `$SHELL` treated as unset.
- **Process names.** `/proc/<pid>/exe`, then `/proc/<pid>/comm`, then `ps` — no `cfg` gate, since
  a `/proc` that is not there fails to open exactly like one that refused. Also removes the
  15-character truncation ceiling on manifest names.

Fixed along the way: `the_command_sink_...` polled for the existence of a file the shell creates
at redirect time rather than for its content, so it raced `cat`'s write. Caught it going red once
in seven Linux runs. That is a CI-reddening flake and Phase 1 is CI.

**Phase 3 — WSL-aware guards — shipped.**

- **Windows drives are detected from the mount table**, not from a `/mnt/` prefix: that prefix is
  configurable (`automount.root`), and the filesystem type — `drvfs`, `9p`, `virtiofs`, `cifs` —
  is the fact that actually matters. Longest matching mount point wins, which is the whole
  subtlety, since `/` prefixes every path on the system.
- **The socket bind is annotated, not pre-empted.** A Windows drive is the likeliest reason a
  bind fails on an otherwise valid path, but likeliest is not certain, and refusing up front
  would break anyone whose mount happens to work. The check therefore costs nothing until
  something has already failed, and then names the one thing the OS error never will. *If Phase 0
  confirms the errno, this can become a pre-flight refusal with a better message.*
- **A repository on a Windows drive earns one toast** at daemon start. It is not broken, only
  slow enough that horde gets the blame for git.
- **`horde status` reports the clock triggers fire on**, offset included — `09:00 UTC+00` on a
  machine whose owner is not on UTC is the tell that the distro's timezone was never set.
  Verified against a real daemon run in a UTC container, which is the same situation.

**Phase 4 — docs — shipped, bar one line.** `docs/wsl.md` is the entry point, registered in
`cli::docs::PAGES` so `horde docs wsl` works from inside a pane, and listed in the docs index.
The README caveat now points at it and states the VM-teardown consequence up front. `ETXTBSY`
sits alongside the macOS SIGKILL entry in troubleshooting and in quick-start's inline comment.
The notification paragraphs in `configuration.md` and `unattended.md` describe what actually
happens per platform, with the WSL `notify_command` recipes.

**The platform badge is deliberately unchanged.** It still reads `macOS · Linux`. A badge is a
claim, and the claim it would be making is the one Phase 0 exists to test. It changes when the
matrix is filled in, not before — as does the "Not yet verified end to end" note in `wsl.md`.

---

## The Phase 0 matrix

For each: does the daemon survive, do the panes survive, does `horde` reattach cleanly, and what
does the user see when it doesn't.

| Scenario | Daemon | Panes | Reattach | Notes |
|---|---|---|---|---|
| Close the Windows Terminal tab | | | | |
| Close every Windows Terminal window | | | | |
| ...then wait 5 minutes | | | | |
| ...then wait 1 hour | | | | |
| Lock the screen | | | | |
| Sleep / resume the laptop | | | | |
| `wsl --terminate <distro>` | | | | |
| `wsl --shutdown` | | | | |
| Windows Update reboot | | | | |
| Repo on `/mnt/c`, `--worktree` fleet | | | | timing, not survival |
| `XDG_CONFIG_HOME` on `/mnt/c` | | | | expected: bind fails — capture errno |

## What we will not claim

Whatever the matrix says, the README caveat should stay narrow. Something in the shape of:
*Windows via WSL2. Native Windows is not planned. `wsl --shutdown` and a Windows reboot end the
session as a machine restart would — the layout comes back, the agents do not.*

## Deliberately out of scope

Native Windows (ConPTY plus named pipes plus losing `SCM_RIGHTS` handoff is a rewrite of the
daemon's most load-bearing trick, for an audience that has WSL). A Windows-side client attaching
to a WSL daemon. Shipping a prebuilt Linux binary — that is a release-engineering question this
repo has not answered for any platform yet.
