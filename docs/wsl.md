# horde on Windows

horde runs on Windows through **WSL2**, as an ordinary Linux binary inside your distro. There is
no `horde.exe` and there is not going to be one — see [Why not a native
build](#why-not-a-native-build) if you want the reasoning rather than the assertion.

Everything in the rest of the documentation applies unchanged. This page is only the places
where Linux-under-Windows is not quite Linux.

---

## Install

Inside your distro, not in PowerShell:

```sh
sudo apt install build-essential git        # Ubuntu; a compiler and git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cd ~/code/horde                              # see "Keep your repositories in Linux" below
cargo build --release
rm -f ~/.local/bin/horde && cp target/release/horde ~/.local/bin/
```

`rm` first is not decoration. Copying onto the binary a running daemon is executing corrupts it;
on Linux the copy itself fails with `Text file busy`, and on macOS it fails later and stranger.
Unlinking first puts the new binary on a fresh inode and leaves the running daemon's mapped image
alone.

Then `horde` in any project directory, exactly as on Linux.

## Keep your repositories in Linux

Put your code under `~` inside the distro — `/home/you/code` — and **not** under `/mnt/c`.

`/mnt/c` is your Windows drive seen through a translation layer. Git across it is many times
slower, and the worst case is the one horde is for: a `--worktree` fleet does a lot of git at
once, and on `/mnt/c` that turns into a wait long enough that horde looks broken rather than
slow. horde warns you once at startup if it notices, because otherwise it takes the blame.

A Windows drive also cannot host the daemon's unix socket. If you point `HORDE_SOCKET` or
`XDG_CONFIG_HOME` at one, the daemon fails to start and says so with that specific reason
attached.

Your Windows files are still there when you want them — `/mnt/c/Users/you/...` — and Explorer can
open the Linux side at `\\wsl$\Ubuntu\home\you`.

## Clipboard

Works, by one of two routes, chosen automatically:

- **`clip.exe`**, if Windows interop is on — which it is by default.
- **OSC 52** otherwise: horde asks the terminal to do the copying. Windows Terminal supports
  this. Nothing needs installing, and it also means copying works over SSH from inside WSL.

If `[interop]` is switched off in your `wsl.conf`, the first route disappears and the second still
works.

## Notifications

`notify = "system"` does nothing under WSL, and horde says so in the daemon log once rather than
failing quietly on every alert. There is no native sink to use: `notify-send` needs a
notification daemon a headless distro does not run, and every Windows-side toaster needs
something installed first. horde will not take that dependency on your behalf.

Use `notify_command` instead — it reaches anything, and under WSL any Windows executable on
`$PATH` counts. The recipes are in [configuration](configuration.md#notifications).

For what it is worth, this only affects the *detached* alert. The in-app toast, the sidebar, the
bus drawer and `horde digest` are all unaffected, and they are how you find out about things
while you are actually at the machine.

## Fonts

The sidebar draws state marks and box-drawing characters. Cascadia Mono — Windows Terminal's
default — does not have all of them; **Cascadia Code NF** does, and ships with Windows Terminal.
Set it in Settings → Profiles → Appearance → Font face if the sidebar looks like boxes.

## Check your timezone

`horde trigger add --at 09:00 ...` means nine o'clock **where you are**. A distro whose timezone
was never set sits on UTC, and scheduled triggers then fire at what looks like a random hour.

```sh
horde status | grep local_time      # e.g. 09:14 UTC+01
```

If the offset is wrong, `sudo dpkg-reconfigure tzdata` fixes it. WSL syncs the Windows timezone
by default on recent versions, so this is mostly a problem on older or hand-built distros.

## What happens when Windows goes away

This is the one place where horde on Windows is genuinely weaker than horde on macOS or Linux,
and it is worth understanding before you rely on it.

horde's central promise is that the daemon outlives your terminal: close the window, the agents
keep working. Inside WSL that holds — the daemon detaches from your session, and a backgrounded
process keeps the distro alive.

What it cannot survive is the **VM** going away, because WSL2 runs your distro in a virtual
machine that Windows can stop:

| | |
|---|---|
| Closing a terminal tab or window | Daemon survives |
| `wsl --shutdown` or `wsl --terminate` | Everything stops |
| A Windows Update reboot | Everything stops |
| Sleep / hibernate | Depends on your machine — assume it may stop |

When the VM stops there is no shutdown signal to react to, so nothing gets a chance to tidy up.
horde writes its layout a second after any change, so **the session comes back** — spaces, tabs,
panes, splits — but the **agents do not**. They were processes, and the machine they were on
stopped. It is the same as a reboot, arriving at times a reboot normally would not.

If you leave a fleet running overnight, turn off Windows Update's automatic restart.

> **Not yet verified end to end.** The sleep/hibernate row in particular is the honest answer
> rather than a tested one. If you find out, the matrix in `PLAN-wsl.md` is where it should land.

## Why not a native build

A `horde.exe` would not help WSL — WSL runs Linux binaries, so it would be a second platform
rather than a shared one, and every Linux fix would still be needed.

It would also cost two of the three things horde is for. horde identifies what is running in a
pane by reading the PTY's foreground process group, and protects long bus messages by checking
the terminal's line discipline. Both are Unix concepts with no ConPTY equivalent — they are not
merely unimplemented in the PTY library horde uses, they are `#[cfg(unix)]` in its trait. And
`horde upgrade` hands live PTYs to a successor daemon over `SCM_RIGHTS`, which a Windows
pseudoconsole cannot be handed the same way.

So: agent detection would degrade from a kernel fact to a heuristic, live upgrade would go, and
WSL would still need everything above. WSL2 is a real Linux kernel; using it is the better trade.

## Troubleshooting

**`horde` starts but the daemon does not.** Check `~/.config/horde/horde.log`. If your config
directory is on a Windows drive, move it: `export HORDE_CONFIG_DIR=$HOME/.config/horde`.

**Everything is slow.** `pwd`. If it starts with `/mnt/`, that is why. See above.

**Panes open a shell that immediately exits.** Check `$SHELL` points at something installed —
horde falls back to `/bin/sh` when it is unset, but honours it when it is set and wrong.

**Agents are not detected.** `horde roster` shows what horde thinks is running. Detection reads
the pane's foreground process, so an agent started through a wrapper script may need a manifest
entry — see [agents](agents.md).
