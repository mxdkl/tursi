#!/bin/sh
python3 -m unittest -q test_geometry || exit 1
python3 - <<'EOF'
import ast, sys
tree = ast.parse(open("geometry.py").read())
bad = []
for f in tree.body:
    if isinstance(f, ast.FunctionDef) and not f.name.startswith("_"):
        doc = ast.get_docstring(f) or ""
        args = [a.arg for a in f.args.args]
        if len(doc) < 30 or not all(a in doc for a in args):
            bad.append(f.name)
print("undocumented:", bad)
sys.exit(1 if bad else 0)
EOF
