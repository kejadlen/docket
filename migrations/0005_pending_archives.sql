-- Finishing is intent too (the Done-archives design): marking a
-- message Done on a writable account queues the poll loop to move it
-- out of the shared Inbox and into Archive, keeping Docket/ labels and
-- folder memberships. The push is self-limiting — mail another client
-- already filed out of the Inbox is left untouched, its row cleared.
CREATE TABLE pending_archives (
    message INTEGER PRIMARY KEY REFERENCES messages (id) ON DELETE CASCADE
) STRICT;
