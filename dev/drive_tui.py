"""Drives sparkline's REPL in a pseudo-terminal and checks what's on screen.

Covers completion (tables, alias columns), Esc, multi-line input, Ctrl-C
cancelling a running Spark job, error carets and meta commands. Exits
non-zero if any check fails. Needs a Spark Connect server (see
dev/with-server.sh) and `pyte`:

    make test-tui
    # or by hand:
    .spark-venv/bin/python dev/drive_tui.py target/debug/sparkline [args...]

Pass -v to print every screen snapshot.
"""

import fcntl
import os
import pty
import select
import signal
import struct
import sys
import tempfile
import termios
import time

import pyte

COLS, ROWS = 110, 40
TIMEOUT_S = 120


def main() -> int:
    args = sys.argv[1:]
    verbose = "-v" in args
    argv = [a for a in args if a != "-v"]
    if not argv:
        print(__doc__)
        return 2

    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)
    failures: list[str] = []

    data_home = tempfile.mkdtemp(prefix="sparkline-tui-")
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["XDG_DATA_HOME"] = data_home  # fresh history
        os.execv(argv[0], argv)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    def pump(seconds: float) -> None:
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.05)
            if not r:
                continue
            try:
                data = os.read(fd, 65536)
            except OSError:
                return
            if not data:
                return
            stream.feed(data)
            # Answer cursor-position queries (ESC[6n) like a real terminal.
            if b"\x1b[6n" in data:
                os.write(fd, f"\x1b[{screen.cursor.y + 1};{screen.cursor.x + 1}R".encode())

    def send(keys: str, wait: float = 0.4) -> None:
        # An escape sequence must arrive in one write, or it's read as
        # separate keys.
        if keys == "\x1b":
            chunks = [keys]
        elif keys.startswith("\x1b"):
            chunks = ["\x1b" + c for c in keys.split("\x1b")[1:]]
        else:
            chunks = list(keys)
        for chunk in chunks:
            os.write(fd, chunk.encode())
            pump(0.01)
        pump(wait)

    def text() -> str:
        lines = [line.rstrip() for line in screen.display]
        while lines and not lines[-1]:
            lines.pop()
        return "\n".join(lines)

    def until(needle: str, timeout: float) -> bool:
        """Pump output until `needle` is on screen."""
        end = time.time() + timeout
        while time.time() < end:
            if needle in text():
                return True
            pump(0.1)
        return needle in text()

    def check(title: str, ok: bool) -> None:
        if verbose or not ok:
            print(f"\n===== {title} =====\n" + "\n".join(text().splitlines()[-24:]))
        print(f"{'ok  ' if ok else 'FAIL'} {title}", flush=True)
        if not ok:
            failures.append(title)

    def watchdog(signum, frame):
        check(f"finished within {TIMEOUT_S}s", False)
        os.kill(pid, signal.SIGKILL)
        sys.exit(1)

    signal.signal(signal.SIGALRM, watchdog)
    signal.alarm(TIMEOUT_S)

    check("startup banner and prompt", until("spark:default >", 30))

    send("CREATE OR REPLACE TEMP VIEW events AS SELECT id AS event_id, id % 3 AS user_id, 'click' AS kind FROM range(10);\r")
    until("OK ·", 30)

    send("SELECT * FROM ev")
    send("\t", 1.0)
    check("Tab completes a table name", "SELECT * FROM events" in text())

    send("\x05\x15")  # Ctrl-E, Ctrl-U: clear the line
    send("SELECT e. FROM events e", 0.2)
    send("\x1b[D" * len(" FROM events e"), 0.2)  # cursor back to just after 'e.'
    send("\t", 1.0)
    shown = text()
    check("Tab after an alias lists its columns", all(c in shown for c in ("event_id", "user_id", "kind")))

    send("\x1b", 0.5)
    check("Esc closes the completion menu", "event_id" not in text().split("SELECT e.")[-1])

    send("\x05\x15")
    send("select kind, count(*) AS n\r", 0.4)
    send("from events group by kind;\r")
    check("multi-line statement runs", until("1 row ·", 30) and "click" in text())

    send("SELECT count(*) FROM range(0, 100000000000, 1, 4) WHERE reflect('java.lang.Thread', 'sleep', 1L) IS NULL;\r")
    check("live progress while a query runs", until("tasks", 30))
    os.write(fd, b"\x03")  # Ctrl-C; the terminal is in cooked mode while a query runs
    check("Ctrl-C cancels the query", until("query cancelled", 20))

    send("SELECT * FROM missing_tbl;\r")
    check("errors show class and caret", until("^^^^^^^^^^^", 20) and "TABLE_OR_VIEW_NOT_FOUND" in text())

    send("explain select * from events;\r")
    check("EXPLAIN prints the plan verbatim", until("== Physical Plan ==", 20) and "↵" not in text())

    send("\\d\r")
    check("\\d lists tables", until("isTemporary", 20))

    send("\x04", 1.5)  # Ctrl-D
    _, status = os.waitpid(pid, 0)
    check("Ctrl-D exits cleanly", os.waitstatus_to_exitcode(status) == 0)

    print(f"\n{len(failures)} failed" if failures else "\nall checks passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
