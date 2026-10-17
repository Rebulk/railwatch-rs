"""Compares capture_rails.rb's and digest-dump's mail lists: same mails in
the same order, every field byte for byte. Prints a unified diff for the
first mismatching field and exits 1 on any difference."""
import difflib
import json
import sys

rails, rust = (json.load(open(p)) for p in sys.argv[1:3])
bad = 0
if len(rails) != len(rust):
    print(f"mail count: rails {len(rails)} rust {len(rust)}")
    bad += 1
for i, (a, b) in enumerate(zip(rails, rust)):
    for field in ("account_id", "to", "subject", "text", "html"):
        if a[field] != b[field]:
            bad += 1
            print(f"mail {i} ({a['to']}): {field} differs")
            if isinstance(a[field], str):
                sys.stdout.writelines(difflib.unified_diff(a[field].splitlines(True), b[field].splitlines(True), "rails", "rust", n=1))
envs = sum(m["text"].count(" job attempts, ") for m in rails)
print(f"{len(rails)} mails, {envs} environment sections: {'IDENTICAL' if bad == 0 else f'{bad} DIFFERENCES'}")
sys.exit(1 if bad else 0)
