#!/bin/bash

# Image name for the Docker container
DOCKER_IMAGE="audioshare"

# Default command to run in the container if no additional commands are provided
DEFAULT_CMD="/bin/bash"

# Check if additional arguments are provided (commands to execute inside the container)
if [ "$#" -eq 0 ]; then
    CMD=$DEFAULT_CMD
else
    CMD="$@"
fi

# Allocate a TTY only when stdin is one. Without this, non-interactive callers
# (to_pi.sh, CI) fail with "the input device is not a TTY".
TTY_FLAGS="-i"
if [ -t 0 ]; then
    TTY_FLAGS="-it"
fi

# Run the Docker container
docker run ${TTY_FLAGS} --rm \
    -v "$(pwd)":/app \
    -w /app \
    ${DOCKER_IMAGE} \
    ${CMD}

