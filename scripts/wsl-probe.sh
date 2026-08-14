#!/usr/bin/env bash
#
# Fills in the Phase 0 survival matrix in PLAN-wsl.md.
#
# horde's central claim is that the daemon outlives your terminal. Under WSL that claim has a
# second kill switch above the one horde controls: Windows can stop the whole VM, and when it
# does there is no signal to react to and nothing to log. The only way to know which actions do
# that is to try each one and look afterwards — which is a memory test unless something writes
# it down. This writes it down.
#
#   ./wsl-probe.sh start          # start an isolated daemon, record what it is
#   <do the thing: close the window, sleep the laptop, wsl --shutdown, ...>
#   ./wsl-probe.sh check "closed every terminal window"
#   ./wsl-probe.sh report         # the matrix, as markdown
#
# The daemon it starts is deliberately isolated — its own socket, config and state under
# ~/.horde-probe — so a real horde session is never the thing being killed.

set -uo pipefail

HORDE="${HORDE:-$(command -v horde || echo "$HOME/.local/bin/horde")}"
PROBE_DIR="$HOME/.horde-probe"
STATE="$PROBE_DIR/probe-state"
RESULTS="$PROBE_DIR/results.tsv"

export HORDE_CONFIG_DIR="$PROBE_DIR/config"
export HORDE_SOCKET="$PROBE_DIR/horde.sock"

# The WSL instance's identity. A new boot_id means the distro was torn down and restarted, which
# is the difference between "the daemon died" and "the machine it was on stopped existing" —
# two very different answers that look identical from a dead socket.
boot_id() { cat /proc/sys/kernel/random/boot_id 2>/dev/null || echo unknown; }
uptime_s() { awk '{printf "%d", $1}' /proc/uptime 2>/dev/null || echo 0; }

need_horde() {
    if [ ! -x "$HORDE" ]; then
        echo "no horde binary at $HORDE — build it first, or set HORDE=/path/to/horde" >&2
        exit 1
    fi
}

cmd_start() {
    need_horde
    mkdir -p "$HORDE_CONFIG_DIR"
    "$HORDE" stop >/dev/null 2>&1
    rm -f "$HORDE_SOCKET"

    setsid "$HORDE" daemon >"$PROBE_DIR/daemon.log" 2>&1 &
    for _ in $(seq 1 60); do [ -S "$HORDE_SOCKET" ] && break; sleep 0.1; done
    if [ ! -S "$HORDE_SOCKET" ]; then
        echo "daemon never bound its socket — see $PROBE_DIR/daemon.log" >&2
        exit 1
    fi

    # Recorded from the socket rather than from `$!`, so a daemon that re-execs or hands off is
    # still identified by the process actually serving.
    local pid panes
    pid=$(pgrep -f "$HORDE daemon" | head -1)
    panes=$("$HORDE" status 2>/dev/null | awk '$1=="panes"{print $2}')

    printf 'pid\t%s\nboot\t%s\nstarted\t%s\nuptime\t%s\npanes\t%s\n' \
        "$pid" "$(boot_id)" "$(date -Is)" "$(uptime_s)" "${panes:-0}" >"$STATE"

    echo "probe daemon up: pid $pid, ${panes:-0} pane(s)"
    echo "now do the thing you want to test, then: $0 check \"what you did\""
}

cmd_check() {
    local label="${1:-unlabelled}"
    if [ ! -f "$STATE" ]; then
        echo "no probe running — $0 start first" >&2
        exit 1
    fi
    local was_pid was_boot started was_panes
    was_pid=$(awk '$1=="pid"{print $2}' "$STATE")
    was_boot=$(awk '$1=="boot"{print $2}' "$STATE")
    started=$(awk '$1=="started"{print $2}' "$STATE")
    was_panes=$(awk '$1=="panes"{print $2}' "$STATE")

    local distro daemon panes
    if [ "$(boot_id)" != "$was_boot" ]; then
        distro="restarted"
    else
        distro="same"
    fi

    # `kill -0` asks the kernel rather than trusting the socket file, which outlives its daemon.
    if kill -0 "$was_pid" 2>/dev/null; then daemon="alive"; else daemon="gone"; fi

    if [ "$daemon" = alive ]; then
        panes=$("$HORDE" status 2>/dev/null | awk '$1=="panes"{print $2}')
        panes="${panes:-unreachable}"
    else
        panes="-"
    fi

    mkdir -p "$PROBE_DIR"
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$label" "$distro" "$daemon" "$was_panes" "$panes" "$started" >>"$RESULTS"

    echo "$label"
    echo "  distro:  $distro (boot_id ${was_boot:0:8} -> $(boot_id | cut -c1-8))"
    echo "  daemon:  $daemon (pid $was_pid)"
    echo "  panes:   $was_panes before, $panes now"
    [ "$daemon" = gone ] && echo "  -> re-run '$0 start' before the next scenario"
    return 0
}

cmd_report() {
    [ -f "$RESULTS" ] || { echo "nothing recorded yet"; exit 0; }
    echo "| Scenario | Distro | Daemon | Panes before | Panes after |"
    echo "|---|---|---|---|---|"
    awk -F'\t' '{printf "| %s | %s | %s | %s | %s |\n", $1, $2, $3, $4, $5}' "$RESULTS"
}

cmd_env() {
    echo "kernel:     $(uname -srm)"
    echo "wsl:        ${WSL_DISTRO_NAME:-<unset>} (interop: $([ -n "${WSL_INTEROP:-}" ] && echo on || echo off))"
    echo "boot_id:    $(boot_id)"
    echo "uptime:     $(uptime_s)s"
    echo "timezone:   $(date +'%Z %z')"
    echo "home fs:    $(stat -f -c %T "$HOME" 2>/dev/null)"
    echo "/mnt/c fs:  $(stat -f -c %T /mnt/c 2>/dev/null || echo '<not mounted>')"
    echo "clip.exe:   $(command -v clip.exe >/dev/null && echo yes || echo no)"
    # The claim PLAN-wsl.md marks as unverified: whether a Windows drive can host a unix socket.
    if [ -d /mnt/c ]; then
        local t="/mnt/c/horde-socket-test.$$"
        if "$HORDE" --version >/dev/null 2>&1 &&
            HORDE_SOCKET="$t" timeout 10 "$HORDE" daemon >"$PROBE_DIR/drvfs.log" 2>&1; then
            echo "drvfs sock: BOUND — the plan's assumption is wrong, say so"
        else
            echo "drvfs sock: refused — $(grep -oE '\(os error [0-9]+\)|Permission denied|Operation not supported' "$PROBE_DIR/drvfs.log" 2>/dev/null | head -1)"
        fi
        rm -f "$t"
    fi
}

mkdir -p "$PROBE_DIR"
case "${1:-}" in
start) cmd_start ;;
check) shift; cmd_check "$@" ;;
report) cmd_report ;;
env) cmd_env ;;
*)
    echo "usage: $0 {start|check \"label\"|report|env}"
    exit 1
    ;;
esac
