import math
import unittest
import geometry as g


class Geometry(unittest.TestCase):
    def test_all(self):
        self.assertAlmostEqual(g.area_circle(2), 4 * math.pi)
        self.assertEqual(g.distance((0, 0), (3, 4)), 5)
        self.assertEqual(g.centroid([(0, 0), (2, 0), (2, 2), (0, 2)]), (1, 1))
        self.assertEqual(g.bounding_box([(1, 5), (-2, 3), (4, -1)]), (-2, -1, 4, 5))


if __name__ == "__main__":
    unittest.main()
