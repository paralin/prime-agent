"""Fail-closed argv gate used by BashHandle on POSIX."""

from __future__ import annotations

import os
import sys


def main() -> None:
    if len(sys.argv) < 2:
        raise SystemExit(125)
    if not sys.stdin.buffer.read(1):
        raise SystemExit(125)
    with open(os.devnull, "rb", buffering=0) as source:
        os.dup2(source.fileno(), 0)
    os.execvpe(sys.argv[1], sys.argv[1:], os.environ)


if __name__ == "__main__":
    main()
