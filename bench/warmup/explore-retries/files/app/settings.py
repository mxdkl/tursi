import os
from .defaults import defaults


def load():
    s = defaults()
    if "APP_RETRIES" in os.environ:
        s["retries"] = int(os.environ["APP_RETRIES"])
    return s
