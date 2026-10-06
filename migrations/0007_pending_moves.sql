-- Every state is a place on the server (DESIGN.md, State), so Done's
-- archive generalizes: a state change on a writable account queues the
-- poll loop to move the message to match — Do, Wait, and Watch out of
-- the Inbox onto their label, Done out of both into Archive, Inbox back.
-- The latest state per message is the only row there is.
CREATE TABLE pending_moves (
    message TEXT NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK (state IN ('inbox', 'do', 'wait', 'watch', 'done')),
    PRIMARY KEY (message)
) STRICT;

INSERT INTO pending_moves (message, state)
SELECT message, 'done' FROM pending_archives;

DROP TABLE pending_archives;
