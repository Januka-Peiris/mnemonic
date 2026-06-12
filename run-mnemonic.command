#!/bin/zsh
# Double-clickable launcher: builds and runs Mnemonic, leaving the
# terminal open on failure so compile errors stay visible.
cd "$(dirname "$0")"
cargo run -p mnemonic
exit_code=$?
if [ $exit_code -ne 0 ]; then
    echo ""
    echo "=== mnemonic exited with status $exit_code ==="
    read -k 1 "?Press any key to close..."
fi
