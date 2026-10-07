import os


def ttl():
    return int(os.environ.get("APP_TOKEN_TTL", "3600"))
