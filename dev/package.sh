#!/usr/bin/env bash
# Builds a release binary and packages it for distribution:
#   dist/sparkline-<target>.tar.gz  (sparkline, README.md, LICENSE, NOTICE)
#   dist/sparkline-<target>.tar.gz.sha256
#
#   dev/package.sh                        # host target
#   dev/package.sh x86_64-apple-darwin    # cross target (rustup target add first)
#
# Asset names carry no version so the "latest release" download URL is stable.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
cargo build --release --locked --target "$TARGET" -p sparkline

NAME="sparkline-$TARGET"
STAGE="dist/$NAME"
rm -rf "$STAGE" "dist/$NAME.tar.gz" "dist/$NAME.tar.gz.sha256"
mkdir -p "$STAGE"
cp "target/$TARGET/release/sparkline" README.md LICENSE NOTICE "$STAGE/"
tar -C dist -czf "dist/$NAME.tar.gz" "$NAME"
(cd dist && shasum -a 256 "$NAME.tar.gz" >"$NAME.tar.gz.sha256")
rm -rf "$STAGE"
echo "dist/$NAME.tar.gz"
