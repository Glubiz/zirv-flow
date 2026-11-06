#!/bin/sh
# Creates the read-only user zirv-flow reads the support database with.
set -eu
mysql -uroot -p"$MYSQL_ROOT_PASSWORD" <<SQL
CREATE DATABASE IF NOT EXISTS support;
CREATE USER 'flow_reader'@'%' IDENTIFIED BY '$FLOW_READER_PASSWORD';
GRANT SELECT ON support.* TO 'flow_reader'@'%';
SQL
