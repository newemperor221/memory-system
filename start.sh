#!/bin/bash
# Start script for memory-system-v2
# Loads config from .env file in the same directory

cd "$(dirname "$0")"

if [ -f .env ]; then
    set -a
    source .env
    set +a
fi

exec ./target/release/memory-system-v2 serve --listen 0.0.0.0:7891 >> /tmp/memory-system-v2.log 2>&1
