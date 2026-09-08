from pg_tsy.tuning.walk_forward import purged_walk_forward, robustness_score


def test_purged_walk_forward_has_gap() -> None:
    folds = purged_walk_forward(100, train_size=40, test_size=10, purge_size=5)
    assert folds
    assert all(fold.test_start - fold.train_end == 5 for fold in folds)


def test_robustness_penalizes_dispersion() -> None:
    stable = robustness_score([1.0, 1.0, 1.0])
    unstable = robustness_score([0.0, 1.0, 2.0])
    assert stable > unstable
