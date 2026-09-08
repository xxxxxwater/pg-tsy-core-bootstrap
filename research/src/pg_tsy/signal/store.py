from __future__ import annotations

from pathlib import Path
from threading import Lock

from .models import Signal


class JsonlSignalStore:
    """Development-only append store.

    Production should use a durable database/stream boundary with explicit consumption semantics.
    """

    def __init__(self, path: str | Path) -> None:
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._lock = Lock()

    def append(self, signal: Signal) -> None:
        with self._lock, self.path.open("a", encoding="utf-8") as f:
            f.write(signal.to_json() + "\n")
