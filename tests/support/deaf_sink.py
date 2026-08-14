# A slave that sets raw mode and then never reads, standing in for a hung agent. Writing to
# this pane blocks once the tty input queue fills, which is the hazard `accepts_input` guards.
import sys, tty, time
try:
    tty.setraw(sys.stdin.fileno())
except Exception:
    pass
# Same readiness marker as raw_sink.py, for the same reason: a test must be able to wait for
# raw mode to exist rather than sleep a guess at python's startup time.
sys.stdout.write("READY\r\n")
sys.stdout.flush()
while True:
    time.sleep(3600)
