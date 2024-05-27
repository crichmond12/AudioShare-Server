#!/bin/bash

# Set your Raspberry Pi's SSH details
PI_USER="crichmond"         # Change this to your Pi's username
PI_HOST="raspberrypi.local"  # Change this to your Pi's hostname or IP address

# SSH command to get the Raspberry Pi's serial number
SERIAL_CMD="cat /proc/cpuinfo | grep Serial | cut -d ' ' -f 2"

# Fetch the serial number from the Raspberry Pi
SERIAL_NUMBER=$(ssh "${PI_USER}@${PI_HOST}" "${SERIAL_CMD}")

# Check if the SSH command was successful
if [ $? -ne 0 ]; then
  echo "Failed to connect to the Raspberry Pi or retrieve the serial number."
  exit 1
fi

echo "Serial number of the Raspberry Pi: ${SERIAL_NUMBER}"

# Generate the QR code
OUTPUT_FILE="pi_serial_qrcode.png"
qrencode -o "${OUTPUT_FILE}" "${SERIAL_NUMBER}"

# Check if qrencode was successful
if [ $? -ne 0 ]; then
  echo "Failed to generate QR code."
  exit 1
fi

echo "QR code generated and saved as ${OUTPUT_FILE}"
