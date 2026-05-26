#!/bin/bash
set -e

# Start PostgreSQL service
/etc/init.d/postgresql start &

# Keep the container running
tail -f /dev/null

