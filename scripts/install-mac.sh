#!/bin/sh
# Installs the ManjiKick worker for this user. No admin password needed.
#   curl -fsSL https://github.com/104316stu/ManjiKickCompareWorker/releases/latest/download/install-mac.sh | sh
set -eu

base="${MKW_BASE:-https://github.com/104316stu/ManjiKickCompareWorker/releases/latest/download}"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading the ManjiKick worker..."
curl -fsSL --retry 3 "$base/mkworker-macos" -o "$tmp/mkworker-macos"
curl -fsSL --retry 3 "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"

want=$(awk '$2 == "mkworker-macos" || $2 == "*mkworker-macos" { print $1 }' "$tmp/SHA256SUMS")
got=$(shasum -a 256 "$tmp/mkworker-macos" | awk '{ print $1 }')
if [ -z "$want" ] || [ "$want" != "$got" ]; then
    echo "The download does not match its checksum; nothing was installed." >&2
    exit 1
fi

chmod +x "$tmp/mkworker-macos"
"$tmp/mkworker-macos" install
echo "Done. Look for its icon in the menu bar."
