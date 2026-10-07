#!/bin/sh
# Passes on the real merge(), fails on at least two of three mutants.
[ -f test_intervals.py ] || { echo "no test_intervals.py"; exit 1; }
python3 -m unittest -q test_intervals || exit 1
killed=0
for m in "$FIXTURE"/mutants/*.py; do
    d=$(mktemp -d); cp test_intervals.py "$d/"; cp "$m" "$d/intervals.py"
    (cd "$d" && python3 -m unittest -q test_intervals >/dev/null 2>&1) || killed=$((killed+1))
    rm -rf "$d"
done
echo "mutants killed: $killed/3"
[ "$killed" -ge 2 ]
