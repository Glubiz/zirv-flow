-- The recipient of a message, as a keyed hash (#223, supervisor ruling 86c6f5a4).
--
-- `0048_messages.sql` says `messages` holds no address. That stays true: this column holds the
-- keyed identifier HMAC of the address the message was actually sent to (the same value the
-- `suppressions` table is keyed by), never the address. It is written when the message is marked
-- `sending`, from the address the send job already hashed for the suppression gate. A provider's
-- feedback webhook (bounce, complaint) reads it by message id to suppress that address, so the
-- address of a person who changed it since is never suppressed by mistake.
--
-- It is null for every message sent before this migration, and for a message that never reached
-- `sending`. Feedback for such a message cannot suppress anyone and is counted, and it never falls
-- back to the person's current address. The column goes with its row when the person is erased.
ALTER TABLE messages ADD COLUMN address_hash bytea
    CHECK (address_hash IS NULL OR octet_length(address_hash) = 32);
