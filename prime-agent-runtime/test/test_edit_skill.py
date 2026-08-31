from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from rlm.bash import bash as bash_module_bash  # noqa: F401  (transport availability)

SKILL = Path(__file__).parents[2] / "packages/coding-agent/skills/edit/src/edit/__init__.py"


def _load_skill():
    spec = importlib.util.spec_from_file_location("edit_skill_test", SKILL)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _fake_ssh(directory: str) -> str:
    path = os.path.join(directory, "ssh")
    with open(path, "w", encoding="utf-8") as stream:
        stream.write(
            "#!/usr/bin/env python3\n"
            "import json, os, subprocess, sys\n"
            "separator = sys.argv.index('--')\n"
            "command = sys.argv[separator + 2]\n"
            "data = sys.stdin.buffer.read()\n"
            "remote_env = {'PATH': os.environ.get('PATH', ''), 'HOME': os.environ.get('HOME', '')}\n"
            "result = subprocess.run(['/bin/sh', '-c', command], input=data, env=remote_env)\n"
            "raise SystemExit(result.returncode)\n"
        )
    os.chmod(path, 0o755)
    return path


class EditSkillTest(unittest.IsolatedAsyncioTestCase):
    async def test_local_edit_replaces_a_unique_string(self) -> None:
        module = _load_skill()
        with tempfile.TemporaryDirectory() as tmp:
            target = os.path.join(tmp, "file.txt")
            with open(target, "w", encoding="utf-8") as stream:
                stream.write("alpha\nold line\nbeta\n")
            message = await module.run(target, "old line", "new line")
            self.assertIn(target, message)
            with open(target, encoding="utf-8") as stream:
                self.assertEqual(stream.read(), "alpha\nnew line\nbeta\n")

    async def test_remote_edit_runs_the_exact_once_check_on_the_host(self) -> None:
        module = _load_skill()
        with tempfile.TemporaryDirectory() as tmp:
            fake = _fake_ssh(tmp)
            target = os.path.join(tmp, "remote file.txt")
            with open(target, "w", encoding="utf-8") as stream:
                stream.write("keep\nold line\nkeep\n")
            with mock.patch.dict(os.environ, {"PATH": os.environ.get("PATH", "")}):
                with mock.patch("rlm.bash._ssh_executable", return_value=fake):
                    message = await module.run(target, "old line", "new line", ssh="core@thumper")
            self.assertIn(target, message)
            with open(target, encoding="utf-8") as stream:
                self.assertEqual(stream.read(), "keep\nnew line\nkeep\n")

    async def test_remote_edit_maps_host_errors_to_local_exception_types(self) -> None:
        module = _load_skill()
        with tempfile.TemporaryDirectory() as tmp:
            fake = _fake_ssh(tmp)
            with mock.patch("rlm.bash._ssh_executable", return_value=fake):
                with self.assertRaisesRegex(FileNotFoundError, "no-such-file"):
                    await module.run("no-such-file", "old", "new", ssh="core@thumper")
                with self.assertRaisesRegex(ValueError, "string not found"):
                    absent = "abse" "nt snippet"  # split so the source text never contains the marker
                    await module.run(__file__, absent, "new", ssh="core@thumper")

    async def test_remote_edit_rejects_ambiguous_matches_without_writing(self) -> None:
        module = _load_skill()
        with tempfile.TemporaryDirectory() as tmp:
            fake = _fake_ssh(tmp)
            target = os.path.join(tmp, "dup.txt")
            with open(target, "w", encoding="utf-8") as stream:
                stream.write("dup\ndup\n")
            with mock.patch("rlm.bash._ssh_executable", return_value=fake):
                with self.assertRaisesRegex(ValueError, "2 occurrences"):
                    await module.run(target, "dup", "x", ssh="core@thumper")
            with open(target, encoding="utf-8") as stream:
                self.assertEqual(stream.read(), "dup\ndup\n")
