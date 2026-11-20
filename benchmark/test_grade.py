import unittest

from grade import validate


class GradeValidationTest(unittest.TestCase):
    def row(self, finding, label, reference):
        return {
            "finding": finding,
            "label": label,
            "reference": reference,
            "reason": "test",
        }

    def test_duplicate_does_not_earn_another_match(self):
        validate([self.row(0, "match", 0), self.row(1, "duplicate", None)], 2, 1)
        with self.assertRaises(ValueError):
            validate([self.row(0, "match", 0), self.row(1, "match", 0)], 2, 1)

    def test_partial_grade_is_invalid(self):
        with self.assertRaises(ValueError):
            validate([self.row(0, "uncertain", None)], 2, 3)

    def test_empty_review_is_complete(self):
        validate([], 0, 3)


if __name__ == "__main__":
    unittest.main()
