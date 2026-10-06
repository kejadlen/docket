-- Deleting is intent, like filing: the route marks the thread Done
-- here, and the poll loop turns rows into Email/destroy (task sm),
-- which Fastmail answers by moving the mail to Trash.
CREATE TABLE pending_deletes (
    message INTEGER PRIMARY KEY REFERENCES messages (id) ON DELETE CASCADE
) STRICT;
