#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
export IPHONEOS_DEPLOYMENT_TARGET=17.0
export SDKROOT="$(xcrun --sdk iphoneos --show-sdk-path)"
export CARGO_TARGET_DIR="$PWD/target"

cargo build --locked --release --target aarch64-apple-ios -p p2p-sync-core --lib

app="ios/out/Payload/P2PSync.app"
module="ios/out/CP2PSync"
mkdir -p "$app" "$module"
cp ios/Info.plist "$app/Info.plist"
cp ios/CP2PSync/module.modulemap core/include/p2p_sync_core.h "$module/"
plutil -lint "$app/Info.plist"

xcrun --sdk iphoneos swiftc -parse-as-library \
    -target arm64-apple-ios17.0 -sdk "$SDKROOT" \
    -I "$module" target/aarch64-apple-ios/release/libp2p_sync_core.a \
    -Xlinker -no_adhoc_codesign \
    ios/SyncCore.swift ios/P2PSyncApp.swift ios/WriterView.swift -o "$app/P2PSync"
test "$(xcrun lipo -archs "$app/P2PSync")" = arm64
xcrun vtool -show-build "$app/P2PSync"

(
    cd ios/out
    rm -f P2PSync-unsigned.ipa
    zip -qr P2PSync-unsigned.ipa Payload
    unzip -tq P2PSync-unsigned.ipa
    shasum -a 256 P2PSync-unsigned.ipa
)
