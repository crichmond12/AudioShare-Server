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

# Run the Docker container
docker run -it --rm \
    -v $(pwd):/app \
    -w /app \
    ${DOCKER_IMAGE} \
    ${CMD}

