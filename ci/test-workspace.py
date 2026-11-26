#!/usr/bin/env python3
"""Exercise the fork's workspace CLI against a disposable Linux mux server."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest


IDLE = [sys.executable, "-c", "import time; time.sleep(120)"]


def await_result(probe, description):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        if result := probe():
            return result
        time.sleep(0.04)
    raise AssertionError(f"timed out waiting for {description}")


def pane(argv=IDLE):
    return {"cwd": None, "argv": argv, "title": "pane"}


def tab(panes, layout=None):
    return {"title": "test", "panes": panes, "layout": layout}


def window(workspace, tabs):
    return {"workspace": workspace, "active_tab": 0, "tabs": tabs}


def split(direction, first_dim, second_dim, first, second):
    def size(cols, rows):
        return {"cols": cols, "rows": rows, "pixel_width": 0, "pixel_height": 0, "dpi": 0}

    return {
        "Split": dict(
            direction=direction,
            first_size=size(*first_dim),
            second_size=size(*second_dim),
            first=first,
            second=second,
        )
    }


class WorkspaceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="wezterm-ws-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        runtime = self.root / "run"
        runtime.mkdir(mode=0o700)
        self.sock = runtime / "mux.sock"
        self.env = dict(os.environ, XDG_RUNTIME_DIR=str(runtime),
                        XDG_DATA_HOME=str(self.root / "data"),
                        XDG_CACHE_HOME=str(self.root / "cache"),
                        XDG_CONFIG_HOME=str(self.root / "config"),
                        WEZTERM_UNIX_SOCKET=str(self.sock))
        self.env.pop("WEZTERM_PANE", None)
        self.env.pop("WEZTERM_CONFIG_FILE", None)
        self.snapshot = self.root / "workspace.json"
        self.wrapper_log = self.root / "wrapper.log"
        self.wrapper = self.root / "wrapper.py"
        self.wrapper.write_text(
            "import os, sys\nfrom pathlib import Path\n"
            f"with Path({str(self.wrapper_log)!r}).open('a') as log: log.write('wrapped\\n')\n"
            "os.execv(sys.argv[1], sys.argv[1:])\n",
            encoding="utf-8",
        )
        self.wrapper_argv = [sys.executable, str(self.wrapper)]
        executable, wrapper = map(json.dumps, self.wrapper_argv)
        config = self.root / "workspace.lua"
        config.write_text(f"""
local wezterm = require 'wezterm'
return {{
  mux_enable_ssh_agent = false,
  initial_cols = 80,
  unix_domains = {{ {{ name = 'workspace-test', socket_path = {json.dumps(str(self.sock))} }} }},
  exec_domains = {{ wezterm.exec_domain('workspace-wrapper', function(cmd)
    table.insert(cmd.args, 1, {wrapper})
    table.insert(cmd.args, 1, {executable})
    return cmd
  end) }},
}}
""", encoding="utf-8")
        with (self.root / "server.log").open("wb") as log:
            self.server = subprocess.Popen([SERVER, "--config-file", config, *IDLE],
                                           cwd=self.root, env=self.env,
                                           stdout=log, stderr=log, start_new_session=True)
        self.addCleanup(self.stop_server)
        original = await_result(self.ready_panes, "the startup pane")
        self.window_id = str(original[0]["window_id"])
        self.original_pane = original[0]["pane_id"]

    def stop_server(self):
        if self.server.poll() is None:
            self.server.terminate()
            try:
                self.server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.server.kill()
                self.server.wait(timeout=5)

    def cli(self, *args, check=True):
        result = subprocess.run([CLI, "--skip-config", "cli", "--no-auto-start", *args],
                                cwd=self.root, env=self.env, capture_output=True,
                                text=True, timeout=15)
        if check and result.returncode:
            self.fail(f"{args}: {result.stdout}\n{result.stderr}")
        return result

    def ready_panes(self):
        if self.server.poll() is not None:
            self.fail((self.root / "server.log").read_text(encoding="utf-8"))
        if self.sock.exists():
            result = self.cli("list", "--format", "json", check=False)
            if result.returncode == 0:
                return json.loads(result.stdout)
        return None

    def panes(self):
        return json.loads(self.cli("list", "--format", "json").stdout)

    def restore(self, windows, *, version=3, check=True):
        self.snapshot.write_text(json.dumps({"version": version, "created_at": "2026-01-01",
                                             "windows": windows}), encoding="utf-8")
        return self.cli("restore-workspace", "--path", self.snapshot,
                        "--window-id", self.window_id, "--replace", check=check)

    def test_save_uses_private_snapshot_and_backup(self):
        self.cli("save-workspace", "--path", self.snapshot)
        original = self.snapshot.read_bytes()
        self.assertEqual(self.snapshot.stat().st_mode & 0o777, 0o600)
        # Existing permissive snapshots must not make subsequent saves public.
        self.snapshot.chmod(0o644)
        self.cli("save-workspace", "--path", self.snapshot)
        backup = self.snapshot.with_name(self.snapshot.name + ".bak")
        self.assertEqual(backup.read_bytes(), original)
        for path in (self.snapshot, backup):
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_failed_backup_does_not_replace_snapshot(self):
        self.cli("save-workspace", "--path", self.snapshot)
        original = self.snapshot.read_bytes()
        backup = self.snapshot.with_name(self.snapshot.name + ".bak")
        backup.mkdir()
        result = self.cli("save-workspace", "--path", self.snapshot, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.snapshot.read_bytes(), original)
        self.assertIn("workspace backup", result.stderr)

    def test_failed_root_and_split_keep_original_pane(self):
        missing = pane([str(self.root / "missing-command")])
        for bad_tab in [tab([missing]), tab([pane(), missing], split(
                "Horizontal", (39, 24), (40, 24), {"Leaf": 0}, {"Leaf": 1}))]:
            with self.subTest(bad_tab=bad_tab):
                result = self.restore([window("review", [bad_tab])], version=2, check=False)
                self.assertEqual([entry["pane_id"] for entry in self.panes()], [self.original_pane])
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("replaced=false", result.stdout)
                self.assertIn("missing-command", result.stderr)

    def test_later_success_cannot_replace_failed_target(self):
        result = self.restore([window("review", [tab([])]), window("other", [tab([pane()])])],
                              version=2, check=False)
        self.assertNotEqual(result.returncode, 0)
        panes = self.panes()
        self.assertTrue(any(entry["pane_id"] == self.original_pane for entry in panes))
        self.assertTrue(any(entry["workspace"] == "other" for entry in panes))

    def test_asymmetric_split_and_workspace_survive_replacement(self):
        inner = split("Vertical", (1, 10), (1, 13), {"Leaf": 0}, {"Leaf": 1})
        layout = split("Horizontal", (1, 24), (148, 24), inner, {"Leaf": 2})
        result = self.restore([window("review", [tab([pane(), pane(), pane()], layout)])])
        self.assertIn("panes=3 failed_panes=0 replaced=true", result.stdout)
        panes = self.panes()
        self.assertEqual(len(panes), 3)
        self.assertNotIn(self.original_pane, {entry["pane_id"] for entry in panes})
        self.assertEqual({entry["workspace"] for entry in panes}, {"review"})

    def test_domain_intent_and_legacy_command_are_wrapped_once(self):
        self.cli("spawn", "--domain-name", "workspace-wrapper", "--window-id", self.window_id, "--", *IDLE)
        self.cli("save-workspace", "--path", self.snapshot)
        saved = json.loads(self.snapshot.read_text(encoding="utf-8"))
        self.assertEqual(saved["version"], 3)
        saved_panes = [entry for item in saved["windows"][0]["tabs"] for entry in item["panes"]]
        domain_pane = next(entry for entry in saved_panes if entry["domain"] == "workspace-wrapper")
        self.assertEqual(domain_pane["argv"], IDLE)
        self.restore([window("review", [tab([domain_pane])])])
        await_result(lambda: self.wrapped(2), "one wrapper call per v3 spawn")
        self.restore([window("review", [tab([pane(self.wrapper_argv + IDLE)])])], version=2)
        await_result(lambda: self.wrapped(3), "one wrapper call for the legacy command")
        self.cli("save-workspace", "--path", self.snapshot)
        saved = json.loads(self.snapshot.read_text(encoding="utf-8"))
        legacy = saved["windows"][0]["tabs"][0]["panes"][0]
        self.assertEqual(legacy["argv"], self.wrapper_argv + IDLE)
        self.assertTrue(legacy["skip_domain_fixup"])

    def wrapped(self, count):
        return self.wrapper_log.exists() and self.wrapper_log.read_text(encoding="utf-8").count("wrapped\n") == count

    def test_close_stops_sending_keys_after_marker_clears(self):
        ready = self.root / "agent-ready"
        received = self.root / "agent-keys"
        script = self.root / "agent.py"
        script.write_text(f"""
import os, select, subprocess, time, tty
from pathlib import Path

tty.setraw(0)
Path({str(ready)!r}).touch()
keys = os.read(0, 1)
subprocess.run([{str(CLI)!r}, '--skip-config', 'cli', '--no-auto-start',
                'set-user-var', '--unset', 'agent.shutdown'], check=True)
while select.select([0], [], [], 0.6)[0]:
    keys += os.read(0, 4096)
Path({str(received)!r}).write_bytes(keys)
time.sleep(10)
""", encoding="utf-8")
        pane_id = self.cli("spawn", "--window-id", self.window_id, "--", sys.executable, script).stdout.strip()
        await_result(ready.exists, "the raw test pane")
        self.cli("set-user-var", "--pane-id", pane_id, "--var", "agent.shutdown=1",
                 "--var", "agent.session_id=test-session")
        result = self.cli("close-agent-sessions", "--timeout-seconds", "4")
        self.assertIn("closed=1 remaining=0", result.stdout)
        await_result(received.exists, "shutdown input capture")
        self.assertEqual(received.read_bytes(), b"\x1b")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: test-workspace.py PATH_TO_WEZTERM PATH_TO_WEZTERM_MUX_SERVER")
    CLI, SERVER = [Path(arg).resolve(strict=True) for arg in sys.argv[1:]]
    unittest.main(argv=[sys.argv[0]], verbosity=2)
