-- JMAP import needs two shapes the fixtures never hit: sent mail from
-- the shared identity has no attributable user, and threads arrive
-- keyed by their JMAP threadId.

-- messages is rebuilt (not ALTERed) because SQLite can't relax a CHECK:
-- sent_by becomes nullable, and received rows must have none.
CREATE TABLE messages_new (
    id INTEGER PRIMARY KEY,
    account TEXT NOT NULL REFERENCES accounts (slug),
    -- The RFC 5322 Message-ID, without angle brackets.
    message_id TEXT NOT NULL,
    -- JMAP's Email id, cached: it can change on reimport.
    jmap_id TEXT,
    thread INTEGER NOT NULL REFERENCES threads (id),
    at TEXT NOT NULL,
    -- JSON arrays of display names or addresses.
    cc TEXT NOT NULL DEFAULT '[]',
    bcc TEXT NOT NULL DEFAULT '[]',
    body TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('received', 'sent')),
    from_name TEXT,
    from_addr TEXT,
    state TEXT CHECK (state IN ('inbox', 'do', 'wait', 'watch', 'done')),
    folder TEXT REFERENCES folders (name),
    -- Null when the sender is the account's shared identity.
    sent_by TEXT REFERENCES users (login),
    sent_to TEXT,
    UNIQUE (account, message_id),
    CHECK (
        (kind = 'received'
            AND from_name IS NOT NULL AND from_addr IS NOT NULL AND state IS NOT NULL
            AND sent_by IS NULL AND sent_to IS NULL)
        OR (kind = 'sent'
            AND sent_to IS NOT NULL
            AND from_name IS NULL AND from_addr IS NULL AND state IS NULL AND folder IS NULL)
    )
) STRICT;

INSERT INTO messages_new SELECT * FROM messages;
DROP TABLE messages;
ALTER TABLE messages_new RENAME TO messages;

CREATE INDEX messages_thread ON messages (thread);
CREATE INDEX messages_state ON messages (state);

-- One thread row per account + JMAP threadId (the ids are opaque and
-- may repeat across accounts); fixtures' NULL thread ids stay many.
CREATE UNIQUE INDEX threads_account_jmap_thread ON threads (account, jmap_thread_id);
