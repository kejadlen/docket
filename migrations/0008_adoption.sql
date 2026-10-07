-- Adopting state changes made in other clients (task qwt). Mailbox
-- moves made elsewhere are user actions, so the poll reads them as a
-- new state, but only when the mailboxes themselves changed: a message
-- the server couldn't move (a lane without its label) must not snap
-- back on its next unrelated update. server_state is the state the
-- mailboxes said when the poll last saw the message, the baseline that
-- change is measured from. NULL until the next sight records one.
ALTER TABLE messages ADD COLUMN server_state TEXT
    CHECK (server_state IN ('inbox', 'do', 'wait', 'watch', 'done'));

-- An adopted change has no Docket user behind it: a NULL user is
-- "via another client". history is rebuilt (not ALTERed) because
-- SQLite can't drop NOT NULL; rowid carries the order across.
CREATE TABLE history_new (
    id TEXT PRIMARY KEY,
    message TEXT NOT NULL REFERENCES messages (id),
    -- NULL when the change came from another client.
    user TEXT REFERENCES users (login),
    at TEXT NOT NULL,
    event TEXT NOT NULL
) STRICT;

INSERT INTO history_new (rowid, id, message, user, at, event)
SELECT rowid, id, message, user, at, event FROM history ORDER BY rowid;

DROP TABLE history;
ALTER TABLE history_new RENAME TO history;

CREATE INDEX history_message ON history (message);
