#!/bin/sh
# The READERS: list must name exactly the three readers of APP_TOKEN.
python3 - "$REPORT" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
if "READERS:" not in text:
    sys.exit("no READERS: list")
tail = text.rsplit("READERS:", 1)[1]
files = {re.sub(r".*/", "", m.group(1)) for m in re.finditer(r"([\w./-]+\.py):\d+", tail)}
want = {"session.py", "main.py", "jobs.py"}
print("listed:", sorted(files))
sys.exit(0 if files == want else 1)
PY
