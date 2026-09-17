import os as _scratch_os
from pathlib import Path as _ScratchPath
from tempfile import NamedTemporaryFile as _ScratchTemporaryFile

class _ScratchEditor:
    def __init__(self, path: str):
        self._path = _ScratchPath(path)

    def read(self) -> str:
        try:
            return self._path.read_text(encoding="utf-8")
        except FileNotFoundError:
            return ""

    def write(self, text: str) -> str:
        if not text.strip():
            raise ValueError("The handoff checkpoint must not be empty")
        self._path.parent.mkdir(parents=True, exist_ok=True)
        temporary = None
        try:
            with _ScratchTemporaryFile(mode="w", encoding="utf-8", dir=self._path.parent, prefix=".scratch-", delete=False) as output:
                temporary = _ScratchPath(output.name)
                output.write(text)
            _scratch_os.replace(temporary, self._path)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
        return "Saved handoff checkpoint"

    def replace(self, old: str, new: str) -> str:
        text = self.read()
        if not old or text.count(old) != 1:
            raise ValueError("scratch_replace requires exactly one occurrence of old; use scratch_read() first")
        return self.write(text.replace(old, new, 1))

_scratch_editor = _ScratchEditor(__PATH__)
scratch_read = _scratch_editor.read
scratch_write = _scratch_editor.write
scratch_replace = _scratch_editor.replace
