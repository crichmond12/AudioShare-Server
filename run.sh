#!/bin/bash

LOG_DEVICE="audioshare_device.log"
LOG_SITE="audioshare_site.log"
LOG_AGENT="dongle_agent.log"

run_device() {
    if [[ "$VERBOSE" == true ]]; then
        ./audioshare_device
    else
        nohup ./audioshare_device >> "$LOG_DEVICE" 2>&1 &
    fi
}

run_agent() {
    if [[ "$VERBOSE" == true ]]; then
        ./dongle_agent
    else
        nohup ./dongle_agent >> "$LOG_AGENT" 2>&1 &
    fi
}

run_site() {
    if [[ "$VERBOSE" == true ]]; then
        ./audioshare_site
    else
        nohup ./audioshare_site >> "$LOG_SITE" 2>&1 &
    fi
}

# Check if verbose flag is set
VERBOSE=false
if [[ "$1" == "-v" ]]; then
    VERBOSE=true
    shift
fi

# Determine which command(s) to run based on arguments
if [[ -z "$1" ]]; then
    # No arguments, run both in the background with logging
    run_device
    run_site
elif [[ "$1" == "device" ]]; then
    # Run only audioshare_device
    run_device
elif [[ "$1" == "site" ]]; then
    # Run only audioshare_site
    run_site
elif [[ "$1" == "agent" ]]; then
    # Run only the dongle agent
    run_agent
else
    echo "Usage: $0 [-v] [device|site|agent]"
    exit 1
fi

