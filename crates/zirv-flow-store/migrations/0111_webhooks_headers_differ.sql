-- A webhook's signature header and timestamp header must differ, compared without regard to case
-- (header names are case-insensitive): the API's `check_scheme` refuses an equal pair, but two
-- PATCHes that each passed it against the same old row (one setting the timestamp header, the
-- other the signature header) could both commit and store one. The rule belongs in the table so
-- no writer can break it; the API maps the violation to the same 422 that `check_scheme` gives.
--
-- UPGRADE COST: ACCESS EXCLUSIVE on `webhooks` for the one scan that validates the constraint.
-- The table has a row per inbound hook, so it is instant. The migration fails only if a stored
-- row already has equal headers, which `check_scheme` forbids on every path except that race;
-- fix such a row (PATCH one header) before upgrading.
ALTER TABLE webhooks ADD CONSTRAINT webhooks_headers_differ_check
    CHECK (timestamp_header IS NULL OR lower(timestamp_header) <> lower(signature_header));
