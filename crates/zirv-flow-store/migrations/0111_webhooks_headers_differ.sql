-- A webhook's signature header and timestamp header must differ, compared without regard to case
-- (header names are case-insensitive): the API's `check_scheme` refuses an equal pair, but two
-- PATCHes that each passed it against the same old row (one setting the timestamp header, the
-- other the signature header) could both commit and store one. The rule belongs in the table so
-- no writer can break it; the API maps the violation to the same 422 that `check_scheme` gives.
--
-- UPGRADE COST: ACCESS EXCLUSIVE on `webhooks` for an instant catalog change. The constraint is
-- added NOT VALID, so the existing rows are not scanned and the migration never fails: an install
-- that already stores a row with equal headers (the very state that race produced) still starts.
-- The constraint is enforced on every insert and update, so such a row keeps working until it is
-- next changed, when the 422 asks for a different header.
ALTER TABLE webhooks ADD CONSTRAINT webhooks_headers_differ_check
    CHECK (timestamp_header IS NULL OR lower(timestamp_header) <> lower(signature_header))
    NOT VALID;
