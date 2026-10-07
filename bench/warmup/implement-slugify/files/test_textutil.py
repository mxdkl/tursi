import unittest
from textutil import slugify


class Slugify(unittest.TestCase):
    def test_basic(self):
        self.assertEqual(slugify("Hello World"), "hello-world")

    def test_punctuation(self):
        self.assertEqual(slugify("  Rust, Go & C++!  "), "rust-go-c")

    def test_runs_collapse(self):
        self.assertEqual(slugify("a---b__c"), "a-b-c")

    def test_accents_fold(self):
        self.assertEqual(slugify("Crème Brûlée"), "creme-brulee")

    def test_nothing_left(self):
        self.assertEqual(slugify("!!!"), "")

    def test_digits_stay(self):
        self.assertEqual(slugify("Top 10 Tips"), "top-10-tips")


if __name__ == "__main__":
    unittest.main()
