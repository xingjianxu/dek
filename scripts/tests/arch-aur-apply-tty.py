"""Run a fixture command on a real terminal and answer known Paru prompts."""

import errno
import os
import pty
import re
import select
import signal
import sys
import time


COMMAND = sys.argv[1:]
PROMPT = re.compile(
    rb"(?:Proceed to review|Accept changes|Proceed with installation)\? \[Y/n\]"
)
ESCAPE = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")


def run():
    pid, terminal = pty.fork()
    if pid == 0:
        os.execvpe(COMMAND[0], COMMAND, {**os.environ, "TERM": "dumb"})

    deadline = time.monotonic() + 180
    pending = b""
    finished = False
    try:
        while time.monotonic() < deadline:
            if not select.select([terminal], [], [], deadline - time.monotonic())[0]:
                continue
            try:
                chunk = os.read(terminal, 4096)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                chunk = b""
            if not chunk:
                _, status = os.waitpid(pid, 0)
                finished = True
                return os.waitstatus_to_exitcode(status)
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()
            pending = ESCAPE.sub(b"", pending + chunk)
            while match := PROMPT.search(pending):
                os.write(terminal, b"y\n")
                pending = pending[match.end() :]
            pending = pending[-512:]
        raise TimeoutError(f"fixture terminal prompt timed out for {COMMAND}")
    finally:
        os.close(terminal)
        if not finished:
            try:
                os.killpg(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)


sys.exit(run())
