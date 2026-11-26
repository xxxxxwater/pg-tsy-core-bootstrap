from __future__ import annotations

import json
import os
import select
import subprocess
from collections.abc import Iterable, Iterator, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Self


@dataclass(slots=True)
class SimRequest:
    order: dict[str, Any]
    top: dict[str, Any]
    position: str | int | float = "0"
    now_ns: int = 0
    session: str = "CONTINUOUS"

    def payload(self, request_id: int) -> dict[str, Any]:
        return {
            "request_id": request_id,
            "order": self.order,
            "top": self.top,
            "position": str(self.position),
            "now_ns": self.now_ns,
            "session": self.session,
        }


class RustSimClient:
    _BATCH_CHUNK = 64

    """Persistent bridge to the Rust matching engine.

    One subprocess is kept alive for an entire experiment. This removes process
    startup from the inner RL/ES interaction loop while preserving a plain JSONL
    protocol that is easy to inspect during strategy development.
    """

    def __init__(self, binary: str | os.PathLike[str] | None = None) -> None:
        binary_path = Path(binary or os.getenv("PG_SIM_BINARY", "target/release/pg-sim"))
        self._process = subprocess.Popen(
            [str(binary_path)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self._next_id = 1

    def close(self) -> None:
        if self._process.poll() is None:
            if self._process.stdin:
                self._process.stdin.close()
            self._process.terminate()
            try:
                self._process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self._process.kill()
                self._process.wait(timeout=2)

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def step(self, request: SimRequest) -> dict[str, Any]:
        if self._process.poll() is not None:
            stderr = self._process.stderr.read() if self._process.stderr else ""
            raise RuntimeError(f"pg-sim exited early: {stderr.strip()}")
        if self._process.stdin is None or self._process.stdout is None:
            raise RuntimeError("pg-sim pipes are unavailable")

        request_id = self._next_id
        self._next_id += 1
        self._process.stdin.write(json.dumps(request.payload(request_id), separators=(",", ":")) + "\n")
        self._process.stdin.flush()

        line = self._process.stdout.readline()
        if not line:
            stderr = self._process.stderr.read() if self._process.stderr else ""
            raise RuntimeError(f"pg-sim returned EOF: {stderr.strip()}")
        response = json.loads(line)
        if response.get("request_id") != request_id:
            raise RuntimeError(
                f"pg-sim response mismatch: expected {request_id}, got {response.get('request_id')}"
            )
        return response["result"]

    def _encoded_windows(self, requests: Iterable[SimRequest]) -> Iterator[list[str]]:
        # Each window starts with an empty input pipe and no outstanding replies.
        # A window within PIPE_BUF fits even if the worker blocks writing its first
        # reply. Larger individual requests and platforms without PIPE_BUF use a
        # single request, so the worker can read the entire line before replying.
        pipe_buf = getattr(select, "PIPE_BUF", 0)
        lines: list[str] = []
        size = 0
        for request_id, request in enumerate(requests, self._next_id):
            payload = json.dumps(request.payload(request_id), ensure_ascii=True, separators=(",", ":"))
            # Each ASCII character is one UTF-8 byte, including the native line ending.
            line_size = len(payload) + len(os.linesep)
            if lines and (size + line_size > pipe_buf or len(lines) == self._BATCH_CHUNK):
                yield lines
                lines = []
                size = 0
            lines.append(payload + "\n")
            size += line_size
        if lines:
            yield lines

    def batch(self, requests: Iterable[SimRequest]) -> list[dict[str, Any]]:
        batch = list(requests)
        if not batch:
            return []
        if self._process.poll() is not None:
            raise RuntimeError("pg-sim exited before batch dispatch")
        if self._process.stdin is None or self._process.stdout is None:
            raise RuntimeError("pg-sim pipes are unavailable")

        results: list[dict[str, Any]] = []
        for lines in self._encoded_windows(batch):
            request_ids = range(self._next_id, self._next_id + len(lines))
            self._next_id += len(lines)
            self._process.stdin.writelines(lines)
            self._process.stdin.flush()

            for expected_id in request_ids:
                line = self._process.stdout.readline()
                if not line:
                    raise RuntimeError("pg-sim returned EOF during batch")
                response = json.loads(line)
                if response.get("request_id") != expected_id:
                    raise RuntimeError(
                        "pg-sim response mismatch: "
                        f"expected {expected_id}, got {response.get('request_id')}"
                    )
                results.append(response["result"])
        return results


class VectorExecutionEnv:
    """Thin vector facade for RL/ES experiments.

    Strategy state/reward remains in Python; exchange execution semantics stay
    in Rust. Keeping the API dependency-light makes it usable from Gymnasium,
    Torch, JAX, Optuna, or custom evolution-strategy loops.
    """

    def __init__(self, client: RustSimClient, size: int) -> None:
        if size <= 0:
            raise ValueError("size must be positive")
        self.client = client
        self.size = size

    def step(self, requests: Sequence[SimRequest]) -> list[dict[str, Any]]:
        if len(requests) != self.size:
            raise ValueError(f"expected {self.size} requests, got {len(requests)}")
        return self.client.batch(requests)
