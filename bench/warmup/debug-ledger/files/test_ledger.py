import unittest
from decimal import Decimal as D
from ledger import split_bill


class SplitBill(unittest.TestCase):
    def test_thirds_of_100(self):
        self.assertEqual(split_bill("100.00", 3), [D("33.34"), D("33.33"), D("33.33")])

    def test_thirds_of_2(self):
        self.assertEqual(split_bill("2.00", 3), [D("0.67"), D("0.67"), D("0.66")])

    def test_even(self):
        self.assertEqual(split_bill("1.00", 4), [D("0.25")] * 4)

    def test_always_sums(self):
        for total in ["0.05", "7.00", "10.01", "99.99"]:
            for n in range(1, 8):
                self.assertEqual(sum(split_bill(total, n)), D(total), (total, n))


if __name__ == "__main__":
    unittest.main()
