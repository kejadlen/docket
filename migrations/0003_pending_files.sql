-- Filing is intent, recorded here and pushed by the poll loop via
-- Email/set (task rn): the routes only write the database, so the UI
-- never blocks on the network. One row per message — the latest
-- filing wins.
CREATE TABLE pending_files (
    message INTEGER PRIMARY KEY REFERENCES messages (id) ON DELETE CASCADE,
    -- NULL unfiles.
    folder TEXT REFERENCES folders (name)
) STRICT;
