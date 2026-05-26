#!/bin/bash

# Function to build Rust project
build_rust() {
		ssh pi "rm ./audioshare_device"
    ./env_exec.sh cargo build
}

# Function to build Go project
build_go() {
		ssh pi "rm ./audioshare_site"
    cd site
		rm audioshare
    ./../env_exec.sh go build
		#cd DB
		#./../../env_exec.sh go build migration.go
    cd ..
}

# Function to send both files in one scp command
send_both_files() {
    echo "Sending both Rust and Go binaries to the target..."
    scp target/debug/audio_share site/audioshare pi:./
}

# Function to send Rust binary
send_rust_file() {
    echo "Sending Rust binary to the target..."
    scp target/debug/audio_share pi:./audioshare_device
}

# Function to send Go binary
send_go_file() {
    echo "Sending Go binary to the target..."
    scp site/audioshare pi:./audioshare_site
		#scp site/DB/migrate	pi:./migrate
}

# Check for parameters
if [ "$1" == "site" ]; then
    echo "Building and sending Go application..."
    build_go
    send_go_file
elif [ "$1" == "device" ]; then
    echo "Building and sending Rust application..."
    build_rust
    send_rust_file
else
    echo "Building and sending both applications..."
    # Build both projects in parallel
    build_rust &  # Run Rust build in the background
    build_go &    # Run Go build in the background
    wait          # Wait for both background tasks to finish

    # Send both files in one scp command
    send_both_files
fi

