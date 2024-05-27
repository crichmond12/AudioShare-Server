#!/bin/bash

directory="./src"

fswatch -0 -x "$directory"/*.rs | while read -d "" event;  do
	echo "$event";
	if [[ $event == *"Update"* ]]; then
		./compile.sh
	fi
done
