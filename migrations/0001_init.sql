-- Docket's own data, plus a cache of the mail it tracks. The mail server
-- stays the source of truth for messages and folders; JMAP import fills
-- these tables and fixtures seed them in dev.

CREATE TABLE users (
    slug TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    -- The Tailscale-User-Login value that identifies this user.
    login TEXT NOT NULL UNIQUE
) STRICT;

CREATE TABLE accounts (
    slug TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    address TEXT NOT NULL,
    read_only INTEGER NOT NULL CHECK (read_only IN (0, 1))
) STRICT;

-- Listed in insertion order, which is the order menus show them.
CREATE TABLE folders (
    name TEXT PRIMARY KEY
) STRICT;

CREATE TABLE threads (
    id INTEGER PRIMARY KEY,
    account TEXT NOT NULL REFERENCES accounts (slug),
    subject TEXT NOT NULL,
    -- JMAP's threadId, cached: it can change on reimport.
    jmap_thread_id TEXT
) STRICT;

-- Received messages carry state and folder; sent ones carry who sent them
-- and to whom. Each column belongs to one kind or the other.
CREATE TABLE messages (
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
    sent_by TEXT REFERENCES users (slug),
    sent_to TEXT,
    UNIQUE (account, message_id),
    CHECK (
        (kind = 'received'
            AND from_name IS NOT NULL AND from_addr IS NOT NULL AND state IS NOT NULL
            AND sent_by IS NULL AND sent_to IS NULL)
        OR (kind = 'sent'
            AND sent_by IS NOT NULL AND sent_to IS NOT NULL
            AND from_name IS NULL AND from_addr IS NULL AND state IS NULL AND folder IS NULL)
    )
) STRICT;

CREATE INDEX messages_thread ON messages (thread);
CREATE INDEX messages_state ON messages (state);

CREATE TABLE assignees (
    message INTEGER NOT NULL REFERENCES messages (id),
    user TEXT NOT NULL REFERENCES users (slug),
    PRIMARY KEY (message, user)
) STRICT;

-- Per-person read tracking; $seen is shared and can't say who read what.
CREATE TABLE reads (
    user TEXT NOT NULL REFERENCES users (slug),
    message INTEGER NOT NULL REFERENCES messages (id),
    PRIMARY KEY (user, message)
) STRICT;

CREATE TABLE comments (
    id INTEGER PRIMARY KEY,
    thread INTEGER NOT NULL REFERENCES threads (id),
    author TEXT NOT NULL REFERENCES users (slug),
    at TEXT NOT NULL,
    text TEXT NOT NULL
) STRICT;

CREATE INDEX comments_thread ON comments (thread);

-- Every change to a message's values, in the words of its undo toast.
CREATE TABLE history (
    id INTEGER PRIMARY KEY,
    message INTEGER NOT NULL REFERENCES messages (id),
    user TEXT NOT NULL REFERENCES users (slug),
    at TEXT NOT NULL,
    event TEXT NOT NULL
) STRICT;

CREATE INDEX history_message ON history (message);
