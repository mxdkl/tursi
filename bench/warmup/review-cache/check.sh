#!/bin/sh
grep -q "VERDICT: buggy" "$REPORT" && grep -Eq "cache\.py:(18|19|20)" "$REPORT"
