"""Drive sparkline's REPL through a pseudo-terminal and print screen snapshots.

Used for manual/CI smoke tests of interactive behaviour (completion menu,
multi-line input, Ctrl-C). Requires `pyte`:

    uv pip install --python .spark-venv/bin/python pyte
    .spark-venv/bin/python dev/drive_tui.py target/debug/sparkline [args...]
"""

import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

import pyte

COLS, ROWS = 110, 40


def main() -> None:
    argv = sys.argv[1:]
    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)

    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["XDG_DATA_HOME"] = os.environ.get("SPARKLINE_TEST_DATA", "/tmp/sparkline-tui-test")
        os.execv(argv[0], argv)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    def pump(seconds: float) -> None:
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return
                if not data:
                    return
                stream.feed(data)
                # Answer cursor-position queries (ESC[6n) like a real terminal.
                if b"\x1b[6n" in data:
                    y, x = screen.cursor.y + 1, screen.cursor.x + 1
                    os.write(fd, f"\x1b[{y};{x}R".encode())

    def send(keys: str, wait: float = 0.4) -> None:
        # Escape sequences must arrive in one write or they're read as
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

    def snap(title: str) -> None:
        lines = [l.rstrip() for l in screen.display]
        while lines and not lines[-1]:
            lines.pop()
        print(f"\n===== {title} =====")
        print("\n".join(lines[-22:]))

    def watchdog(signum, frame):
        snap("WATCHDOG: still running after 90 s")
        os.kill(pid, signal.SIGKILL)
        sys.exit(1)

    signal.signal(signal.SIGALRM, watchdog)
    signal.alarm(90)

    pump(2.0)
    snap("startup")

    send("CREATE OR REPLACE TEMP VIEW events AS SELECT id AS event_id, id % 3 AS user_id, 'click' AS kind FROM range(10);\r", 3)
    send("SELECT * FROM ev")
    send("\t", 1.0)
    snap("tab after 'FROM ev'")

    send("\x15")  # Ctrl-U clear line
    send("SELECT e.", 0.2)
    send(" FROM events e", 0.2)
    send("\x1b[D" * len(" FROM events e"), 0.2)  # move cursor back after 'e.'
    send("\t", 1.0)
    snap("tab after 'e.' (alias columns)")

    send("\x1b", 0.5)  # close menu
    snap("after Esc (menu should be closed)")
    send("\x05\x15")  # Ctrl-E, Ctrl-U: clear the whole line
    snap("after Ctrl-U (line should be empty)")
    send("select kind, count(*) AS n\r", 0.4)
    send("from events group by kind;\r", 3)
    snap("multi-line query")

    send("SELECT count(*) FROM range(0, 100000000000, 1, 4) WHERE reflect('java.lang.Thread', 'sleep', 1L) IS NULL;\r", 2.5)
    snap("long query running")
    os.write(fd, b"\x03")  # Ctrl-C (terminal is in cooked mode while a query runs)
    pump(4)
    snap("after Ctrl-C")

    send("SELECT * FROM missing_tbl;\r", 2)
    snap("error")

    send("\\d\r", 2)
    snap("\\d")

    send("\x04", 1.5)  # Ctrl-D
    _, status = os.waitpid(pid, 0)
    print(f"\nexit status: {os.waitstatus_to_exitcode(status)}")


if __name__ == "__main__":
    main()
