#!/usr/bin/env python3
"""Print something into a pane, then stay alive.

Stands in for an agent that has printed the line a feature triggers on — a usage-limit notice,
a rate-limit refusal, an environment variable — in the tests that need horde to *see* that line.

Why not `echo`. A program that prints and exits immediately is a race on macOS, which discards
whatever is still in a tty's output buffer when the last file descriptor on the slave side
closes. horde reads the master from its own thread, so the sequence that loses the text is:
child writes, child exits, slave closes, kernel drops the buffer, reader thread finally gets
scheduled and finds a closed pty with nothing in it. Under load — a test suite running nine
hundred tests across every core, several of them driving ptys of their own — that thread can
lose the race, and the test then fails saying the feature did not fire when the truth is that
the line never arrived to fire it.

Sleeping keeps the slave open, which is what makes the output arrive every time rather than
almost every time. It is also what a real agent does: prints, and stays.

    python3 say.py rate limit exceeded     # prints "rate limit exceeded"
    python3 say.py --env HORDE_ENV_TEST    # prints that variable's value

Arguments are taken as separate words because `build_command` splits the pane's command on
whitespace and does not honour quotes, so a single quoted argument cannot be expressed at all.
"""

import os
import sys
import time

args = sys.argv[1:]
if len(args) >= 2 and args[0] == "--env":
    print(os.environ.get(args[1], ""), flush=True)
else:
    print(" ".join(args), flush=True)

# Long enough that no test outlives it, and irrelevant either way: every caller kills the pane.
time.sleep(600)
