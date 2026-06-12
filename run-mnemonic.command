#!/bin/zsh
# Double-clickable launcher: builds and runs Mnemonic, leaving the
# terminal open on failure so compile errors stay visible.
cd "$(dirname "$0")"
cargo run -p mnemonic
status=$?
if [ $status -ne 0 ]; then
    echo ""
    echo "=== mnemonic exited with status $status ==="
    read -k 1 "?Press any key to close..."
fi
