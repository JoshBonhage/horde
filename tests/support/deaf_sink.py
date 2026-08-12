# A slave that sets raw mode and then never reads, standing in for a hung agent. Writing to
# this pane blocks once the tty input queue fills, which is the hazard `accepts_input` guards.
import sys, tty, time
try:
    tty.setraw(sys.stdin.fileno())
except Exception:
    pass
while True:
    time.sleep(3600)
