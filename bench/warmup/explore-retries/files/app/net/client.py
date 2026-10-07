import time
from .. import settings


class Client:
    def __init__(self, transport):
        self.transport = transport
        self.retries = settings.load()["retries"]

    def get(self, url):
        for attempt in range(self.retries + 1):
            try:
                return self.transport(url)
            except IOError:
                time.sleep(0.1 * attempt)
        raise IOError(f"gave up on {url}")
