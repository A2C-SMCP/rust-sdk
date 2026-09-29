#!/usr/bin/env python3
"""Run Cargo/nextest executables outside large macOS artifact directories (#229)."""
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile


def run(argv):
    source = Path(argv[0]).resolve(strict=True)
    child = None
    pending = []

    def forward(signum, _frame):
        if child is None:
            pending.append(signum)
        else:
            child.send_signal(signum)

    signals = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
    previous = {sig: signal.signal(sig, forward) for sig in signals}
    try:
        with tempfile.TemporaryDirectory(prefix="rust-sdk-exec-") as directory:
            executable = Path(directory) / source.name
            try:
                # nextest invokes the runner once per test. A hard link gives each process
                # its own directory without copying the same large binary thousands of times.
                os.link(source, executable)
            except OSError:
                shutil.copy2(source, executable)
            # Keep Cargo's cwd, environment, inherited file descriptors and argument boundaries.
            child = subprocess.Popen([str(executable), *argv[1:]], close_fds=False)
            for sig in pending:
                child.send_signal(sig)
            result = child.wait()
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)
    if result < 0:
        # Preserve signal termination after our temporary directory has been cleaned up.
        sig = -result
        if sig != signal.SIGKILL:
            signal.signal(sig, signal.SIG_DFL)
        os.kill(os.getpid(), sig)
    return result


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit("usage: macos-runner.py EXECUTABLE [ARG ...]")
    sys.exit(run(sys.argv[1:]))
