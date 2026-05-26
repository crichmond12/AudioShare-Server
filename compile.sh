#!/bin/bash
#cargo zigbuild --release --target armv7-unknown-linux-gnueabihf
cargo build --release
#scp target/armv7-unknown-linux-gnueabihf/release/audio_share pi:./
scp target/release/audio_share pi:./
#scp audioshare.db pi:./
