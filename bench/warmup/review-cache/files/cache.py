import time


class Cache:
    """A dict whose entries expire after `ttl` seconds."""

    def __init__(self, ttl):
        self.ttl = ttl
        self.items = {}

    def put(self, key, value):
        self.items[key] = (value, time.monotonic())

    def get(self, key, default=None):
        entry = self.items.get(key)
        if entry is None:
            return default
        value, stored = entry
        if time.monotonic() - stored < self.ttl:
            del self.items[key]
            return default
        return value
