#!/bin/bash
# Start SSH agent
ssh-agent -s

# Add default SSH private key
ssh-add ~/.ssh/id_rsa
