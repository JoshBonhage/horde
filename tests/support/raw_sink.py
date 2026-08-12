# A raw-mode slave standing in for an agent TUI: no line discipline, and a small pause per
# read so the tty buffer fills the way it does against an agent that is busy rendering.
# Reports a running byte total so a test can prove nothing was discarded.
import sys, tty, os, time
fd = sys.stdin.fileno()
try:
    tty.setraw(fd)
except Exception:
    pass
total = 0
while True:
    b = os.read(fd, 4096)
    if not b:
        break
    total += len(b)
    time.sleep(0.01)
    sys.stdout.write("GOT=%d\r\n" % total)
    sys.stdout.flush()
