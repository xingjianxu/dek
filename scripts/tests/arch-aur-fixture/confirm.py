"""Answer Paru's actual terminal prompts in a disposable test guest."""
import os
from pathlib import Path
import re
import selectors
import shlex
import subprocess
import sys
import time


run = Path(sys.argv[1])
command = shlex.join(["env", "LC_ALL=C", "paru", "-S", "--aur", *sys.argv[2:]])
proc = subprocess.Popen(
    ["ssh", "-tt", "-F", str(run / "ssh.conf"), "dek-vm", command],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.STDOUT,
)
os.set_blocking(proc.stdout.fileno(), False)
selector = selectors.DefaultSelector()
selector.register(proc.stdout, selectors.EVENT_READ)
deadline = time.monotonic() + 90
prompt = re.compile(rb"(?:Proceed to review|Accept changes|Proceed with installation)\? \[Y/n\]")
pending = b""
try:
    while time.monotonic() < deadline:
        ready = selector.select(deadline - time.monotonic())
        if not ready:
            continue
        chunk = os.read(proc.stdout.fileno(), 4_096)
        if not chunk:
            raise SystemExit(proc.wait(timeout=5))
        sys.stdout.buffer.write(chunk)
        sys.stdout.buffer.flush()
        pending += chunk
        while match := prompt.search(pending):
            proc.stdin.write(b"y\n")
            proc.stdin.flush()
            pending = pending[match.end():]
        pending = pending[-512:]
    raise TimeoutError(f"SSH terminal prompt timed out for {command}")
finally:
    if proc.poll() is None:
        proc.kill()
        proc.wait()
