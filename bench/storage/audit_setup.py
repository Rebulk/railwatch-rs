#!/usr/bin/env python3
"""Control rows for the index audit (bench/storage/index_audit.sh): the
props-diff user as an owner of the environment's account, a second member,
a second application, and the API and MCP diffs' tokens, written straight
into a Rust data dir's control.sqlite3 (no Rails). Writes
DATA_DIR/mcp-tokens.json and DATA_DIR/api-tokens.json and prints the
member's user id.

    audit_setup.py DATA_DIR

Token names follow bench/diff/{mcp,api}/setup_rails.rb. A token's digest
is sha256 hex of the raw token (control::digest).
"""
import hashlib, json, os, secrets, sqlite3, sys

d = sys.argv[1]
db = sqlite3.connect(os.path.join(d, "control.sqlite3"))
now = "2026-11-01 00:00:00.000000"
acct, app = db.execute("SELECT a.account_id, e.application_id FROM environments e JOIN applications a ON a.id = e.application_id ORDER BY e.id LIMIT 1").fetchone()


def user(email, name):
    db.execute("INSERT OR IGNORE INTO users (created_at, updated_at, email, name, password_digest, verified) VALUES (?, ?, ?, ?, 'x', 1)",
               (now, now, email, name))
    return db.execute("SELECT id FROM users WHERE email = ?", (email,)).fetchone()[0]


owner = user("props-diff@example.com", "Props Diff")
member = user("api-diff-member@example.com", "Member")
for uid, role in [(owner, "owner"), (member, "member")]:
    db.execute("INSERT OR IGNORE INTO memberships (account_id, user_id, role, created_at, updated_at) VALUES (?, ?, ?, ?, ?)", (acct, uid, role, now, now))
if not db.execute("SELECT 1 FROM applications WHERE slug = 'mcp-diff-other'").fetchone():
    cols = {r[1]: r for r in db.execute("PRAGMA table_info(applications)")}
    row = dict(zip([c for c in cols], db.execute(f"SELECT * FROM applications WHERE id = ?", (app,)).fetchone()))
    row.pop("id")
    row.update(slug="mcp-diff-other", name="mcp diff other", issue_prefix="OTH")
    db.execute(f"INSERT INTO applications ({', '.join(row)}) VALUES ({', '.join('?' for _ in row)})", list(row.values()))
other = db.execute("SELECT id FROM applications WHERE slug = 'mcp-diff-other'").fetchone()[0]

ALL = ["api", "mcp", "read", "write"]
SPECS = {
    "full": (ALL, None, None, False), "read_only": (["mcp", "read", "api"], None, None, False), "api_only": (["api", "read", "write"], None, None, False),
    "mcp_no_read": (["mcp", "write"], None, None, False), "revoked": (ALL, None, None, True), "other_app": (ALL, None, other, False),
    "account_only": (["api", "read"], acct, None, False), "mcp_only": (["mcp", "read"], None, None, False), "no_read": (["api", "write"], None, None, False),
    "expired": (ALL, None, None, False), "security": (ALL + ["security"], None, None, False),
}
tokens = {}
for name, (scopes, account_id, app_id, revoked) in SPECS.items():
    raw = "rwp_" + secrets.token_hex(20)
    db.execute("INSERT INTO api_tokens (user_id, name, scopes, account_id, application_id, expires_at, revoked_at, token_digest, created_at, updated_at) "
               "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
               (owner, f"audit-{name}", json.dumps(scopes), account_id, app_id, "2000-01-01 00:00:00" if name == "expired" else None,
                now if revoked else None, hashlib.sha256(raw.encode()).hexdigest(), now, now))
    tokens[name] = raw
db.commit()
for f in ("mcp-tokens.json", "api-tokens.json"):
    json.dump(tokens, open(os.path.join(d, f), "w"))
print(member)
