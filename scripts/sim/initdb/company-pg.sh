#!/bin/sh
# Creates the read-only role zirv-flow reads the company database with.
set -eu
psql -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" <<SQL
CREATE ROLE flow_reader LOGIN PASSWORD '$FLOW_READER_PASSWORD';
GRANT CONNECT ON DATABASE $POSTGRES_DB TO flow_reader;
GRANT USAGE ON SCHEMA public TO flow_reader;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO flow_reader;
SQL
