from os import environ

HEADERS = {"Authorization": "Bearer " + environ.get('APP_TOKEN', '')}
