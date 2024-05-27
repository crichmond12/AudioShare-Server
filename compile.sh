#!/bin/bash
cargo zigbuild --target armv7-unknown-linux-gnueabihf
scp target/armv7-unknown-linux-gnueabihf/debug/audio_share pi:./
