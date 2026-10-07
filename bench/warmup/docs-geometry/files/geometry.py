import math


def area_circle(radius):
    return math.pi * radius * radius


def distance(p, q):
    return math.hypot(q[0] - p[0], q[1] - p[1])


def centroid(points):
    n = len(points)
    return (sum(x for x, _ in points) / n, sum(y for _, y in points) / n)


def bounding_box(points):
    xs = [x for x, _ in points]
    ys = [y for _, y in points]
    return (min(xs), min(ys), max(xs), max(ys))
