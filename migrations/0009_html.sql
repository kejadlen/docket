-- HTML mail renders as HTML (task uylo): the message's first text/html
-- part, raw as the server sent it. Sanitizing happens when it's served,
-- so a tightened sanitizer covers mail already stored. body stays the
-- text that lists, search, and quote folding read. NULL for text-only
-- mail, and for mail imported before this until its next reimport.
ALTER TABLE messages ADD COLUMN html TEXT;
