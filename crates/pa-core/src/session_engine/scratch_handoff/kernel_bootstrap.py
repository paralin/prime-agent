import ast as _scratch_ast
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

    def execute(self, code: str) -> None:
        allowed = {"scratch_read": (self.read, 0), "scratch_write": (self.write, 1), "scratch_replace": (self.replace, 2)}
        harmless = {"print": print, "len": len, "repr": repr, "sorted": sorted}
        calls = []
        for statement in _scratch_ast.parse(code).body:
            call = statement.value if isinstance(statement, _scratch_ast.Expr) else None
            if not isinstance(call, _scratch_ast.Call) or not isinstance(call.func, _scratch_ast.Name):
                raise ValueError("Scratch closeout accepts only scratch_read(), scratch_write(text), scratch_replace(old, new)")
            if call.func.id in harmless:
                if call.keywords:
                    raise ValueError("Harmless calls accept literal constants and scratch_read() arguments only")
                calls.append((harmless[call.func.id], [self._harmless_argument(arg) for arg in call.args]))
                continue
            if call.func.id not in allowed:
                raise ValueError("Scratch closeout accepts only scratch_read(), scratch_write(text), scratch_replace(old, new)")
            function, arity = allowed[call.func.id]
            if call.keywords or len(call.args) != arity or any(not isinstance(arg, _scratch_ast.Constant) or not isinstance(arg.value, str) for arg in call.args):
                raise ValueError("Use positional literal strings only; no paths, expressions, or working-kernel variables")
            calls.append((function, [arg.value for arg in call.args]))
        for function, args in calls:
            result = function(*args)
            if result is not None:
                print(result)

    def _harmless_argument(self, arg):
        if isinstance(arg, _scratch_ast.Constant):
            return arg.value
        if (
            isinstance(arg, _scratch_ast.Call)
            and isinstance(arg.func, _scratch_ast.Name)
            and arg.func.id == "scratch_read"
            and not arg.keywords
            and not arg.args
        ):
            return self.read()
        raise ValueError("Harmless calls accept literal constants and scratch_read() arguments only")

_scratch_execute = _ScratchEditor(__PATH__).execute
