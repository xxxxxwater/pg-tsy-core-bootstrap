import unittest
from unittest.mock import patch

from pg_tsy.tuning.walk_forward import WalkForwardFold, purged_walk_forward


class WalkForwardStepTests(unittest.TestCase):
    def test_negative_step_rejected_before_any_fold(self):
        # Bound the old implementation without allowing an unbounded allocation.
        with (
            patch("pg_tsy.tuning.walk_forward.WalkForwardFold",
                  side_effect=AssertionError("fold created before input validation")),
            self.assertRaisesRegex(ValueError, "step_size"),
        ):
            purged_walk_forward(100, train_size=40, test_size=10, step_size=-1)

    def test_negative_step_rejected_even_when_no_fold_fits(self):
        with self.assertRaisesRegex(ValueError, "step_size"):
            purged_walk_forward(10, train_size=40, test_size=10, step_size=-5)

    def test_none_and_zero_keep_default_step(self):
        expected = [WalkForwardFold(i, i + 40, i + 45, i + 55)
                    for i in range(0, 46, 10)]
        for step in (None, 0):
            with self.subTest(step=step):
                self.assertEqual(purged_walk_forward(
                    100, train_size=40, test_size=10, purge_size=5,
                    step_size=step), expected)

    def test_positive_step_is_finite_and_forward(self):
        folds = purged_walk_forward(
            100, train_size=40, test_size=10, purge_size=5, step_size=15)
        self.assertEqual([f.train_start for f in folds], [0, 15, 30, 45])
        self.assertEqual(folds[-1].test_end, 100)

    def test_first_fold_does_not_fit(self):
        self.assertEqual(purged_walk_forward(
            49, train_size=40, test_size=10, step_size=1), [])


if __name__ == "__main__":
    unittest.main()
