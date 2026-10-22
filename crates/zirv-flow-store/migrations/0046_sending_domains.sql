-- Sending domains, their DNS records and sender identities (architecture 10.2 and 16.2).
--
-- A workspace registers a domain it sends from. Zirv Flow generates the domain's DKIM key and
-- keeps the private half sealed in encrypted_secrets, so only the public half is ever published or
-- returned. A domain is `verified` only while its DKIM, SPF and DMARC records all resolve as
-- expected. A sender identity is a from name and address on one such domain; an email template
-- publishes only with a verified sender.
--
-- None of these tables is person-scoped. `sender_identities.from_address` is the address of a
-- sender the workspace owns, never a contact's, so the erasure scan has nothing to find here.

-- A DKIM private key is a sealed secret of its own purpose. Every existing purpose is listed
-- again: the CHECK is replaced, not extended.
ALTER TABLE encrypted_secrets DROP CONSTRAINT encrypted_secrets_purpose_check;
ALTER TABLE encrypted_secrets ADD CONSTRAINT encrypted_secrets_purpose_check
    CHECK (purpose IN ('connector_credential', 'oauth_token', 'ai_provider_key', 'webhook_secret',
                       'webhook_signing', 'destination_credential', 'dkim_key'));

CREATE TABLE sending_domains (
    id               uuid PRIMARY KEY,
    workspace_id     uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- Lower case, at least two labels; the API normalizes it with core's `parse_domain`.
    domain           text NOT NULL CHECK (char_length(domain) BETWEEN 3 AND 253
                                          AND domain = lower(domain)),
    selector         text NOT NULL CHECK (char_length(selector) BETWEEN 1 AND 63),
    -- The sealed PKCS#8 private key. Never read by an API; the SMTP destination opens it to sign.
    dkim_secret_id   uuid NOT NULL,
    -- The public key, base64 SubjectPublicKeyInfo: the `p=` value of the DKIM record.
    dkim_public_key  text NOT NULL CHECK (char_length(dkim_public_key) > 0),
    status           text NOT NULL DEFAULT 'unverified' CHECK (status IN ('unverified', 'verified')),
    -- When the domain last became verified; kept while it stays verified.
    verified_at      timestamptz,
    last_checked_at  timestamptz,
    created_by       uuid REFERENCES users (id),
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    -- A verified domain says since when; an unverified one never claims it.
    CHECK (status <> 'verified' OR verified_at IS NOT NULL),
    FOREIGN KEY (workspace_id, dkim_secret_id) REFERENCES encrypted_secrets (workspace_id, id)
);

-- A workspace registers a domain once.
CREATE UNIQUE INDEX sending_domains_domain_idx ON sending_domains (workspace_id, domain);
CREATE INDEX sending_domains_secret_idx ON sending_domains (workspace_id, dkim_secret_id);
-- The leader's periodic re-check reads the least recently checked domains first.
CREATE INDEX sending_domains_checked_idx ON sending_domains (workspace_id, last_checked_at);

-- The three records a domain must publish and what the last check found.
CREATE TABLE sending_domain_records (
    workspace_id  uuid NOT NULL,
    domain_id     uuid NOT NULL,
    kind          text NOT NULL CHECK (kind IN ('dkim', 'spf', 'dmarc')),
    -- The recommended text; the SPF text carries the mechanism the check requires.
    expected_text text NOT NULL,
    status        text NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'ok', 'mismatch', 'missing')),
    -- The TXT records the last check read at the record's name, bounded by the store.
    observed_text text[] NOT NULL DEFAULT '{}',
    checked_at    timestamptz,
    PRIMARY KEY (domain_id, kind),
    FOREIGN KEY (workspace_id, domain_id) REFERENCES sending_domains (workspace_id, id)
        ON DELETE CASCADE
);

CREATE INDEX sending_domain_records_workspace_idx
    ON sending_domain_records (workspace_id, domain_id);

CREATE TABLE sender_identities (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    domain_id    uuid NOT NULL,
    from_name    text NOT NULL CHECK (char_length(from_name) BETWEEN 1 AND 128),
    -- Lower case, on the identity's domain. The store checks the domain when it writes; this
    -- CHECK keeps the shape.
    from_address text NOT NULL CHECK (char_length(from_address) BETWEEN 3 AND 254
                                      AND from_address = lower(from_address)
                                      AND position('@' IN from_address) > 1),
    reply_to     text CHECK (reply_to IS NULL OR (char_length(reply_to) BETWEEN 3 AND 254
                                                  AND position('@' IN reply_to) > 1)),
    archived_at  timestamptz,
    created_by   uuid REFERENCES users (id),
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    -- Deleting a domain removes its archived identities; it refuses while a template still names
    -- one (the template foreign key below) and, in the store, while a live identity exists.
    FOREIGN KEY (workspace_id, domain_id) REFERENCES sending_domains (workspace_id, id)
        ON DELETE CASCADE
);

-- A live from address is unique in its workspace; an archived one is free again.
CREATE UNIQUE INDEX sender_identities_address_idx ON sender_identities (workspace_id, from_address)
    WHERE archived_at IS NULL;
CREATE INDEX sender_identities_domain_idx ON sender_identities (workspace_id, domain_id);

-- A template's sender identity is now a real reference. No identity existed before this
-- migration, so every value written before it dangled; clear them, then add the key. MATCH
-- SIMPLE: a template with no sender is allowed.
UPDATE templates SET sender_identity_id = NULL WHERE sender_identity_id IS NOT NULL;
ALTER TABLE templates ADD CONSTRAINT templates_sender_identity_fkey
    FOREIGN KEY (workspace_id, sender_identity_id) REFERENCES sender_identities (workspace_id, id);
CREATE INDEX templates_sender_identity_idx ON templates (workspace_id, sender_identity_id);
