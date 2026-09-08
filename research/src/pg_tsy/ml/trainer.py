from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import numpy as np

from .local_runtime import detect_accelerator


@dataclass(frozen=True, slots=True)
class TrainingResult:
    artifact_path: Path
    device: str
    final_loss: float
    samples: int
    features: int


def train_mlp_classifier(
    features: np.ndarray,
    labels: np.ndarray,
    artifact_path: str | Path,
    *,
    epochs: int = 50,
    hidden_dim: int = 64,
    learning_rate: float = 1e-3,
    seed: int = 7,
) -> TrainingResult:
    """Train a small local baseline MLP and persist its state dict.

    This function lazily imports PyTorch so AWS/live installs do not need it.
    """
    try:
        import torch
        from torch import nn
    except ImportError as exc:
        raise RuntimeError("install the local training extra: pip install -e '.[train]'") from exc

    x_np = np.asarray(features, dtype=np.float32)
    y_np = np.asarray(labels, dtype=np.int64)
    if x_np.ndim != 2 or y_np.ndim != 1 or len(x_np) != len(y_np):
        raise ValueError("features must be 2D and labels must be a matching 1D array")
    if len(x_np) < 2:
        raise ValueError("at least two samples are required")

    torch.manual_seed(seed)
    accelerator = detect_accelerator().name
    device = torch.device(accelerator)

    mean = x_np.mean(axis=0, keepdims=True)
    std = x_np.std(axis=0, keepdims=True)
    std[std < 1e-8] = 1.0
    x_np = (x_np - mean) / std

    x = torch.as_tensor(x_np, device=device)
    y = torch.as_tensor(y_np, device=device)
    classes = int(y.max().item()) + 1
    model = nn.Sequential(
        nn.Linear(x.shape[1], hidden_dim),
        nn.ReLU(),
        nn.Linear(hidden_dim, classes),
    ).to(device)
    optimizer = torch.optim.AdamW(model.parameters(), lr=learning_rate)
    loss_fn = nn.CrossEntropyLoss()

    final_loss = 0.0
    model.train()
    for _ in range(epochs):
        optimizer.zero_grad(set_to_none=True)
        logits = model(x)
        loss = loss_fn(logits, y)
        loss.backward()
        optimizer.step()
        final_loss = float(loss.detach().cpu().item())

    path = Path(artifact_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    torch.save(
        {
            "state_dict": model.state_dict(),
            "mean": mean,
            "std": std,
            "input_dim": int(x.shape[1]),
            "classes": classes,
            "hidden_dim": hidden_dim,
        },
        path,
    )
    return TrainingResult(
        artifact_path=path,
        device=accelerator,
        final_loss=final_loss,
        samples=len(x_np),
        features=x_np.shape[1],
    )
