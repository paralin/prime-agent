"""Exact single-occurrence string replacement for existing files."""

from __future__ import annotations

import base64
import json
from pathlib import Path


async def run(
    path: str,
    old_str: str,
    new_str: str,
    *,
    ssh: str | list[str] | None = None,
    ssh_options: list[str] | None = None,
) -> str:
    """Replace a unique string in a file.

    ``old_str`` must appear exactly once in the file at ``path``; that match is
    replaced with ``new_str`` and the file is written back in place. Prefer this
    over rewriting a whole file for targeted edits.

    With ``ssh`` set, the file lives on that remote host: the same exact-once
    check runs there through the ``rlm.bash`` SSH transport, and ``path`` is a
    remote path. Remote edits require ``python3`` on the host.

    Args:
        path: File to edit, relative to the working directory, absolute, or
            `~`-prefixed (the leading `~`/`~user` is expanded to the home dir).
            With ``ssh``, a remote path.
        old_str: Exact text to find. Must occur exactly once in the file.
        new_str: Replacement text.
        ssh: Optional SSH destination, as in ``rlm.bash``. A list of hosts is a
            proxy chain ending at the target host.
        ssh_options: Extra OpenSSH argv entries for ``ssh``.

    Returns:
        A short confirmation message.

    Raises:
        FileNotFoundError: If ``path`` does not exist.
        ValueError: If ``old_str`` is absent or matches more than once.
    """
    if ssh is not None:
        return await _run_remote(path, old_str, new_str, ssh, ssh_options)
    filepath = Path(path).expanduser()
    if not filepath.exists():
        raise FileNotFoundError(f"{path} not found")
    content = filepath.read_text(encoding="utf-8")
    count = content.count(old_str)
    if count == 0:
        raise ValueError(f"string not found in {path}")
    if count > 1:
        raise ValueError(
            f"found {count} occurrences in {path}, need exactly 1 — "
            "widen the snippet to make it unique"
        )
    match_index = content.index(old_str)
    start_line = content.count("\n", 0, match_index) + 1
    filepath.write_text(content.replace(old_str, new_str, 1), encoding="utf-8")
    resolved_path = str(filepath.resolve())
    _emit_diff(resolved_path, old_str, new_str, start_line)
    return f"Edited {resolved_path}"


_REMOTE_PROGRAM = r"""
import base64, json, os, sys
spec = json.loads(base64.b64decode(%(spec)r).decode("ascii"))
path = spec["path"]
old = base64.b64decode(spec["old"]).decode("utf-8")
new = base64.b64decode(spec["new"]).decode("utf-8")
try:
    with open(path, encoding="utf-8") as handle:
        content = handle.read()
except OSError as error:
    print(json.dumps({"ok": False, "kind": "missing", "error": str(error)}))
    sys.exit(1)
count = content.count(old)
if count == 0:
    print(json.dumps({"ok": False, "kind": "absent", "error": "string not found in " + path}))
    sys.exit(1)
if count > 1:
    print(json.dumps({"ok": False, "kind": "ambiguous", "error": "found %%d occurrences in %%s, need exactly 1" %% (count, path)}))
    sys.exit(1)
start_line = content.count(chr(10), 0, content.index(old)) + 1
with open(path, "w", encoding="utf-8") as handle:
    handle.write(content.replace(old, new, 1))
print(json.dumps({"ok": True, "path": os.path.realpath(path), "start_line": start_line}))
"""


def _remote_edit_script(path: str, old_str: str, new_str: str) -> str:
    """Build the exact-once replacement program for a remote host."""
    spec = base64.b64encode(
        json.dumps(
            {
                "path": path,
                "old": base64.b64encode(old_str.encode("utf-8")).decode("ascii"),
                "new": base64.b64encode(new_str.encode("utf-8")).decode("ascii"),
            }
        ).encode("utf-8")
    ).decode("ascii")
    return "python3 - <<'PRIME_EDIT'\n" + (_REMOTE_PROGRAM % {"spec": spec}) + "\nPRIME_EDIT\n"


async def _run_remote(
    path: str,
    old_str: str,
    new_str: str,
    ssh: str,
    ssh_options: list[str] | None,
) -> str:
    try:
        from rlm.bash import bash
    except ImportError:
        raise RuntimeError("edit(ssh=...) requires the rlm bash transport; use it inside the agent kernel") from None
    result = await bash(_remote_edit_script(path, old_str, new_str), ssh=ssh, ssh_options=ssh_options)
    lines = result.output.strip().splitlines()
    report = json.loads(lines[-1]) if lines else None
    if not isinstance(report, dict):
        raise RuntimeError(
            f"remote edit on {ssh} produced no report (exit {result.exit_code}): {result.output.strip()[-400:]}"
        )
    if not report.get("ok"):
        if report.get("kind") == "missing":
            raise FileNotFoundError(report.get("error", f"{path} not found"))
        raise ValueError(report.get("error", f"remote edit failed on {path}"))
    resolved_path = report["path"]
    _emit_diff(resolved_path, old_str, new_str, report["start_line"])
    return f"Edited {resolved_path}"


# Keep in sync with DIFF_DISPLAY_MIME in src/core/kernel/index.ts.
_DIFF_DISPLAY_MIME = "application/vnd.prime-agent.diff+json"


def _emit_diff(path: str, old_str: str, new_str: str, start_line: int) -> None:
    """Stream a diff to the host as a display event; best-effort outside the kernel."""
    try:
        from rlm import emit

        emit(
            {
                _DIFF_DISPLAY_MIME: {
                    "path": path,
                    "old_str": old_str,
                    "new_str": new_str,
                    "start_line": start_line,
                },
                "text/plain": f"Edited {path}",
            }
        )
    except Exception:
        pass
