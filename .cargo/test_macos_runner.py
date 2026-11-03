"""Contract tests for the executable-isolation runner; no Rust compilation required."""
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import unittest

RUNNER = Path(__file__).with_name("macos-runner.py").resolve()


class RunnerTests(unittest.TestCase):
    def test_child_sigkill_and_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            program = Path(directory) / "kill-self"
            program.write_text("#!/usr/bin/env python3\nimport os,sys,signal\n"
                               "print(sys.argv[0],flush=True)\nos.kill(os.getpid(),signal.SIGKILL)\n")
            program.chmod(0o755)
            result = subprocess.run([str(RUNNER), str(program)],
                                    capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, -signal.SIGKILL)
            self.assertFalse(Path(result.stdout.strip()).parent.exists())
            self.assertEqual(result.stderr, "")

    def test_arguments_environment_cwd_exit_and_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            program = root / "program with spaces"
            program.write_text("#!/usr/bin/env python3\nimport json,os,sys\n"
                               "print(json.dumps([sys.argv,os.getcwd(),os.environ['RUNNER_TEST']]))\n"
                               "sys.exit(17)\n")
            program.chmod(0o755)
            result = subprocess.run([str(RUNNER), str(program), "a b", "", "$literal"],
                                    cwd=root, env={**os.environ, "RUNNER_TEST": "kept"},
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 17)
            argv, cwd, value = json.loads(result.stdout)
            self.assertEqual(argv[1:], ["a b", "", "$literal"])
            self.assertEqual(Path(cwd).resolve(), root.resolve())
            self.assertEqual(value, "kept")
            self.assertNotEqual(Path(argv[0]).parent, root)
            self.assertFalse(Path(argv[0]).parent.exists())
            self.assertTrue(program.exists())

    def test_signal_forwarding_and_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            program = Path(directory) / "wait"
            program.write_text("#!/usr/bin/env python3\nimport sys,signal\n"
                               "print(sys.argv[0],flush=True)\nsignal.pause()\n")
            program.chmod(0o755)
            process = subprocess.Popen([str(RUNNER), str(program)], stdout=subprocess.PIPE, text=True)
            try:
                executable = Path(process.stdout.readline().strip())
                process.send_signal(signal.SIGTERM)
                self.assertEqual(process.wait(timeout=5), -signal.SIGTERM)
                self.assertFalse(executable.parent.exists())
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                process.stdout.close()


if __name__ == "__main__":
    unittest.main()
