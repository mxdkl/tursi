#!/bin/sh
grep -q "defaults.py" "$REPORT" && grep -Eq "(^|[^0-9.])5([^0-9]|$)" "$REPORT"
