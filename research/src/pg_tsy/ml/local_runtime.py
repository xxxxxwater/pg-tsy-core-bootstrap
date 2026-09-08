from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class Accelerator:
    name: str
    available: bool


def detect_accelerator() -> Accelerator:
    """Detect the best local accelerator without making torch a base dependency."""
    try:
        import torch
    except ImportError:
        return Accelerator(name="cpu", available=True)

    if torch.cuda.is_available():
        return Accelerator(name="cuda", available=True)
    mps = getattr(torch.backends, "mps", None)
    if mps is not None and mps.is_available():
        return Accelerator(name="mps", available=True)
    return Accelerator(name="cpu", available=True)
