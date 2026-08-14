# A raw-mode slave standing in for an agent TUI: no line discipline, and a small pause per
# read so the tty buffer fills the way it does against an agent that is busy rendering.
# Reports a running byte total so a test can prove nothing was discarded.
import sys, tty, os, time
fd = sys.stdin.fileno()
try:
    tty.setraw(fd)
except Exception:
    pass
# Announce that the line discipline is now raw, so a test can wait for the fact rather than
# guess at how long python takes to start. On a loaded CI runner that guess is wrong, the
# message is delivered into a still-canonical tty, and MAX_CANON silently eats most of it.
sys.stdout.write("READY\r\n")
sys.stdout.flush()
total = 0
while True:
    b = os.read(fd, 4096)
    if not b:
        break
    total += len(b)
    time.sleep(0.01)
    sys.stdout.write("GOT=%d\r\n" % total)
    sys.stdout.flush()
