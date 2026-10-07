import os
import sys


def main():
    if not os.getenv("APP_TOKEN"):
        sys.exit("APP_TOKEN is not set")
