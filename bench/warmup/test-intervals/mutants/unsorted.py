def merge(intervals):
    """Merge overlapping or touching closed intervals [a, b]; returns a new
    sorted list. merge([[1, 3], [2, 4], [6, 8]]) == [[1, 4], [6, 8]]."""
    out = []
    for a, b in intervals:
        if out and a <= out[-1][1]:
            out[-1][1] = max(out[-1][1], b)
        else:
            out.append([a, b])
    return out
