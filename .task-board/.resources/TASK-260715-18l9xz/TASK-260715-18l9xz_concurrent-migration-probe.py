"""Probe: does the runner's version read outside a transaction let two
processes both apply the same migration?

Mirrors migrate::run/apply exactly:
    current = PRAGMA user_version        <-- read, no transaction
    tx = conn.transaction()              <-- DEFERRED
    tx.execute_batch(migration sql)
    INSERT INTO schema_history(version)
    PRAGMA user_version = 2
    commit
"""
import os, sqlite3, tempfile

path = os.path.join(tempfile.mkdtemp(), "probe.sqlite3")

def conn():
    c = sqlite3.connect(path, isolation_level=None, timeout=5.0)
    c.execute("PRAGMA journal_mode=WAL")
    return c

# --- a v1 file, as ensure_schema leaves it --------------------------------
boot = conn()
boot.executescript("""
CREATE TABLE messages (chat_id INTEGER, message_id INTEGER);
INSERT INTO messages VALUES (1, 1);
CREATE TABLE schema_history (version INTEGER NOT NULL PRIMARY KEY,
                             applied_at_ms INTEGER NOT NULL);
INSERT INTO schema_history VALUES (1, 0);
PRAGMA user_version = 1;
""")
boot.close()

A, B = conn(), conn()

# Both processes open at the same moment: run() reads the version first.
va = A.execute("PRAGMA user_version").fetchone()[0]
vb = B.execute("PRAGMA user_version").fetchone()[0]
print(f"A sees v{va}, B sees v{vb} -> both decide to apply migration v2")

def apply_v2(c, who):
    c.execute("BEGIN")                       # rusqlite conn.transaction() == DEFERRED
    c.execute("ALTER TABLE messages ADD COLUMN render_hint TEXT")
    c.execute("INSERT INTO schema_history VALUES (2, 0)")
    c.execute("PRAGMA user_version = 2")
    c.execute("COMMIT")
    print(f"  {who}: applied v2, committed")

apply_v2(A, "A")

try:
    apply_v2(B, "B")
    print("  B: applied v2 as well  <-- would be silent double-apply")
except sqlite3.Error as e:
    print(f"  B: FAILED -> {type(e).__name__}: {e}")
    print("     ^ B's StateStore::open() returns Err(MigrationFailed) on a healthy v2 file")

final = conn()
print(f"\nfile ends at v{final.execute('PRAGMA user_version').fetchone()[0]}, "
      f"schema_history={final.execute('SELECT version FROM schema_history ORDER BY version').fetchall()}")
print(f"columns={[r[1] for r in final.execute('PRAGMA table_info(messages)')]}")
