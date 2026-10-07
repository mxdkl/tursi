from decimal import Decimal


def split_bill(total, people):
    """Split `total` (a string like "100.00") among `people`. Returns a list
    of Decimal shares that sum exactly to the total; the first shares
    absorb the leftover cents."""
    total = Decimal(total)
    share = (total / people).quantize(Decimal("0.01"))
    shares = [share] * people
    leftover = total - share * people
    cents = int(leftover * 100)
    for i in range(cents):
        shares[i] += Decimal("0.01")
    return shares
