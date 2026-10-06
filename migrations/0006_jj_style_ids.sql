-- jj-style ids replace the INTEGER PRIMARY KEYs (task uu): random
-- 16-character reverse-hex strings — jj's change-id alphabet, where
-- nibble 0 renders as z. SQLite can't change a primary key in place,
-- so each table is rebuilt, as 0002 did for messages; an `old` column
-- carries the numeric id across the rebuilds and comes off at the end.
--
-- The id expression is hex(randomblob(8)) with every digit substituted:
-- 0123456789abcdef -> zyxwvutsrqponmlk. Nested replace() calls are safe
-- here because no substituted character is ever substituted again.

CREATE TABLE threads_new (
    id TEXT PRIMARY KEY,
    account TEXT NOT NULL REFERENCES accounts (slug),
    subject TEXT NOT NULL,
    jmap_thread_id TEXT,
    old INTEGER NOT NULL
) STRICT;

INSERT INTO threads_new (id, account, subject, jmap_thread_id, old)
SELECT replace(replace(replace(replace(replace(replace(replace(replace(
       replace(replace(replace(replace(replace(replace(replace(replace(
       lower(hex(randomblob(8))),
       '0','z'),'1','y'),'2','x'),'3','w'),'4','v'),'5','u'),'6','t'),'7','s'),
       '8','r'),'9','q'),'a','p'),'b','o'),'c','n'),'d','m'),'e','l'),'f','k'),
       account, subject, jmap_thread_id, id
FROM threads;

DROP TABLE threads;
ALTER TABLE threads_new RENAME TO threads;

CREATE TABLE messages_new (
    id TEXT PRIMARY KEY,
    account TEXT NOT NULL REFERENCES accounts (slug),
    message_id TEXT NOT NULL,
    jmap_id TEXT,
    thread TEXT NOT NULL REFERENCES threads (id),
    at TEXT NOT NULL,
    cc TEXT NOT NULL DEFAULT '[]',
    bcc TEXT NOT NULL DEFAULT '[]',
    body TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('received', 'sent')),
    from_name TEXT,
    from_addr TEXT,
    state TEXT CHECK (state IN ('inbox', 'do', 'wait', 'watch', 'done')),
    folder TEXT REFERENCES folders (name),
    sent_by TEXT REFERENCES users (login),
    sent_to TEXT,
    old INTEGER NOT NULL,
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

INSERT INTO messages_new (id, account, message_id, jmap_id, thread, at, cc, bcc, body,
    kind, from_name, from_addr, state, folder, sent_by, sent_to, old)
SELECT replace(replace(replace(replace(replace(replace(replace(replace(
       replace(replace(replace(replace(replace(replace(replace(replace(
       lower(hex(randomblob(8))),
       '0','z'),'1','y'),'2','x'),'3','w'),'4','v'),'5','u'),'6','t'),'7','s'),
       '8','r'),'9','q'),'a','p'),'b','o'),'c','n'),'d','m'),'e','l'),'f','k'),
       m.account, m.message_id, m.jmap_id, t.id, m.at, m.cc, m.bcc, m.body,
       m.kind, m.from_name, m.from_addr, m.state, m.folder, m.sent_by, m.sent_to, m.id
FROM messages m JOIN threads t ON t.old = m.thread;

DROP TABLE messages;
ALTER TABLE messages_new RENAME TO messages;

CREATE TABLE assignees_new (
    message TEXT NOT NULL REFERENCES messages (id),
    user TEXT NOT NULL REFERENCES users (login),
    PRIMARY KEY (message, user)
) STRICT;

INSERT INTO assignees_new (message, user)
SELECT m.id, a.user FROM assignees a JOIN messages m ON m.old = a.message;

DROP TABLE assignees;
ALTER TABLE assignees_new RENAME TO assignees;

CREATE TABLE reads_new (
    user TEXT NOT NULL REFERENCES users (login),
    message TEXT NOT NULL REFERENCES messages (id),
    PRIMARY KEY (user, message)
) STRICT;

INSERT INTO reads_new (user, message)
SELECT r.user, m.id FROM reads r JOIN messages m ON m.old = r.message;

DROP TABLE reads;
ALTER TABLE reads_new RENAME TO reads;

CREATE TABLE comments_new (
    id TEXT PRIMARY KEY,
    thread TEXT NOT NULL REFERENCES threads (id),
    author TEXT NOT NULL REFERENCES users (login),
    at TEXT NOT NULL,
    text TEXT NOT NULL
) STRICT;

INSERT INTO comments_new (id, thread, author, at, text)
SELECT replace(replace(replace(replace(replace(replace(replace(replace(
       replace(replace(replace(replace(replace(replace(replace(replace(
       lower(hex(randomblob(8))),
       '0','z'),'1','y'),'2','x'),'3','w'),'4','v'),'5','u'),'6','t'),'7','s'),
       '8','r'),'9','q'),'a','p'),'b','o'),'c','n'),'d','m'),'e','l'),'f','k'),
       t.id, c.author, c.at, c.text
FROM comments c JOIN threads t ON t.old = c.thread;

DROP TABLE comments;
ALTER TABLE comments_new RENAME TO comments;

CREATE TABLE history_new (
    id TEXT PRIMARY KEY,
    message TEXT NOT NULL REFERENCES messages (id),
    user TEXT NOT NULL REFERENCES users (login),
    at TEXT NOT NULL,
    event TEXT NOT NULL
) STRICT;

-- Random ids carry no order, so the insertion order history needs lives
-- in the rowid the new table already has.
INSERT INTO history_new (rowid, id, message, user, at, event)
SELECT h.rowid,
       replace(replace(replace(replace(replace(replace(replace(replace(
       replace(replace(replace(replace(replace(replace(replace(replace(
       lower(hex(randomblob(8))),
       '0','z'),'1','y'),'2','x'),'3','w'),'4','v'),'5','u'),'6','t'),'7','s'),
       '8','r'),'9','q'),'a','p'),'b','o'),'c','n'),'d','m'),'e','l'),'f','k'),
       m.id, h.user, h.at, h.event
FROM history h JOIN messages m ON m.old = h.message
ORDER BY h.rowid;

DROP TABLE history;
ALTER TABLE history_new RENAME TO history;

-- The queued-intent tables point at messages too, so they take the
-- same rebuild.
CREATE TABLE pending_files_new (
    message TEXT NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    folder TEXT REFERENCES folders (name),
    PRIMARY KEY (message)
) STRICT;

INSERT INTO pending_files_new (message, folder)
SELECT m.id, p.folder FROM pending_files p JOIN messages m ON m.old = p.message;

DROP TABLE pending_files;
ALTER TABLE pending_files_new RENAME TO pending_files;

CREATE TABLE pending_deletes_new (
    message TEXT NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    PRIMARY KEY (message)
) STRICT;

INSERT INTO pending_deletes_new (message)
SELECT m.id FROM pending_deletes p JOIN messages m ON m.old = p.message;

DROP TABLE pending_deletes;
ALTER TABLE pending_deletes_new RENAME TO pending_deletes;

CREATE TABLE pending_archives_new (
    message TEXT NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    PRIMARY KEY (message)
) STRICT;

INSERT INTO pending_archives_new (message)
SELECT m.id FROM pending_archives p JOIN messages m ON m.old = p.message;

DROP TABLE pending_archives;
ALTER TABLE pending_archives_new RENAME TO pending_archives;

CREATE UNIQUE INDEX threads_account_jmap_thread ON threads (account, jmap_thread_id);
CREATE INDEX messages_thread ON messages (thread);
CREATE INDEX messages_state ON messages (state);
CREATE INDEX comments_thread ON comments (thread);
CREATE INDEX history_message ON history (message);

ALTER TABLE threads DROP COLUMN old;
ALTER TABLE messages DROP COLUMN old;
