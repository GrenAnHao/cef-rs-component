"""Windows-only regression for the Cargo smoke script, not a CEF smoke test.

A fake cargo exits while its own disposable child remains alive. The smoke
script must wait for Cargo, not that descendant; failures/timeouts must still
fail. Every fixture process is verified by executable path before cleanup.
"""
import ctypes
from ctypes import wintypes as wt
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
KERNEL = ctypes.WinDLL("kernel32", use_last_error=True)
KERNEL.OpenProcess.argtypes = [wt.DWORD, wt.BOOL, wt.DWORD]
KERNEL.OpenProcess.restype = wt.HANDLE
KERNEL.QueryFullProcessImageNameW.argtypes = [wt.HANDLE, wt.DWORD, wt.LPWSTR, ctypes.POINTER(wt.DWORD)]
KERNEL.WaitForSingleObject.argtypes = [wt.HANDLE, wt.DWORD]
KERNEL.WaitForSingleObject.restype = wt.DWORD
KERNEL.TerminateProcess.argtypes = [wt.HANDLE, wt.UINT]
KERNEL.CloseHandle.argtypes = [wt.HANDLE]


def fixture_alive(pid, executable, terminate=False):
    handle = KERNEL.OpenProcess(0x1000 | 0x100000 | 1, False, pid)
    if not handle:
        return False
    try:
        path = ctypes.create_unicode_buffer(32768)
        length = wt.DWORD(len(path))
        if not KERNEL.QueryFullProcessImageNameW(handle, 0, path, ctypes.byref(length)):
            return False
        if os.path.normcase(path.value) != os.path.normcase(str(executable)):
            raise AssertionError("Refusing to touch a PID whose executable changed")
        alive = KERNEL.WaitForSingleObject(handle, 0) == 258
        if alive and terminate:
            if not KERNEL.TerminateProcess(handle, 1):
                raise ctypes.WinError(ctypes.get_last_error())
            if KERNEL.WaitForSingleObject(handle, 5000) != 0:
                raise AssertionError("Fixture process failed to exit")
        return alive
    finally:
        KERNEL.CloseHandle(handle)


class CargoSmokeWaitTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        (ROOT / "target").mkdir(exist_ok=True)
        cls.temp = tempfile.TemporaryDirectory(prefix="cargo wait fixture ", dir=ROOT / "target")
        cls.directory = Path(cls.temp.name).resolve()
        cls.cargo = cls.directory / "cargo.exe"
        subprocess.run(
            ["rustc", "--edition", "2024", str(ROOT / "scripts/test-cargo-run-wait.rs"), "-o", str(cls.cargo)],
            cwd=ROOT, check=True,
        )
        cls.saved_logs = {}
        for name in ("cargo-run.stdout.log", "cargo-run.stderr.log"):
            path = ROOT / "target" / name
            cls.saved_logs[path] = path.read_bytes() if path.exists() else None

    @classmethod
    def tearDownClass(cls):
        for path, data in cls.saved_logs.items():
            if data is None:
                path.unlink(missing_ok=True)
            else:
                path.write_bytes(data)
        cls.temp.cleanup()

    def run_smoke(self, mode, timeout_seconds=None):
        for name in ("parent.pid", "child.pid"):
            (self.directory / name).unlink(missing_ok=True)
        env = os.environ.copy()
        env["PATH"] = str(self.directory) + os.pathsep + env["PATH"]
        env["G0_CARGO_WAIT_DIR"] = str(self.directory)
        env["G0_CARGO_WAIT_MODE"] = mode
        command = ["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(ROOT / "scripts/test-cargo-run.ps1")]
        if timeout_seconds is not None:
            command += ["-TimeoutSeconds", str(timeout_seconds)]
        output_path = self.directory / (mode + "-wrapper.log")
        with output_path.open("wb") as output_file:
            process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=output_file, stderr=subprocess.STDOUT)
            try:
                try:
                    process.wait(timeout=12)
                except subprocess.TimeoutExpired:
                    process.terminate()  # Only this test's PowerShell, never its whole descendant tree.
                    process.wait(timeout=5)
                    self.fail("Smoke script waited for a descendant instead of Cargo: " + output_path.read_text(errors="replace"))
                result = (process.returncode, output_path.read_text(errors="replace"))
                if mode == "success":
                    child = self.directory / "child.pid"
                    self.assertTrue(child.exists(), "The fixture must actually spawn a child")
                    self.assertTrue(fixture_alive(int(child.read_text()), self.cargo), "Descendant exited before wait behavior was tested")
                return result
            finally:
                for name in ("parent.pid", "child.pid"):
                    path = self.directory / name
                    if path.exists():
                        fixture_alive(int(path.read_text()), self.cargo, terminate=True)

    def test_success_does_not_wait_for_descendant(self):
        code, output = self.run_smoke("success")
        self.assertEqual(code, 0, output)
        self.assertIn("Cargo startup regression passed.", output)

    def test_cargo_failure_remains_failure(self):
        code, output = self.run_smoke("failure")
        self.assertNotEqual(code, 0, output)
        self.assertIn("cargo run failed with exit code 37", output)

    def test_timeout_is_bounded_and_reported(self):
        code, output = self.run_smoke("timeout", timeout_seconds=1)
        self.assertNotEqual(code, 0, output)
        self.assertIn("cargo run timed out after 1 seconds", output)


if __name__ == "__main__":
    unittest.main()
