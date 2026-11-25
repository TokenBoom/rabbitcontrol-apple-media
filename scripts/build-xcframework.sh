#!/usr/bin/env bash
# 构建 rc-mobile-ffi + rc-media-apple 静态库并打包为 RabbitControlCore.xcframework。
# 本地与 CI 共用;core 依赖走 Cargo.toml 的 git+tag,如需本地 patch 提供 .cargo/config.toml。
#
# 用法: ./scripts/build-xcframework.sh [输出目录]
set -euo pipefail

MEDIA_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TARGETS="${RC_TARGETS:-aarch64-apple-ios aarch64-apple-ios-sim}"
OUT_DIR="${1:-$MEDIA_DIR/build}"
XCFW_DIR="$OUT_DIR/RabbitControlCore.xcframework"
# Keep native Cargo dependencies and the Metal object on the same minimum iOS version.
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-13.0}"

if ! command -v cargo >/dev/null 2>&1; then
    arch="$(uname -m)"
    [[ "$arch" == "arm64" ]] && arch="aarch64"
    TC="$HOME/.rustup/toolchains/stable-$arch-apple-darwin/bin"
    [[ -x "$TC/cargo" ]] && export PATH="$TC:$PATH"
fi

libs=()
for target in $TARGETS; do
    echo "==> cargo build --release --target $target"
    (cd "$MEDIA_DIR" && cargo build --release -p rc-mobile-ffi --target "$target")
    (cd "$MEDIA_DIR" && cargo build --release -p rc-media-apple --target "$target")
    core_lib="$MEDIA_DIR/target/$target/release/librc_mobile_ffi.a"
    merged="$(dirname "$core_lib")/librabbitcontrol_combined.a"
    case "$target" in
        aarch64-apple-ios) sdk=iphoneos; triple="arm64-apple-ios${IPHONEOS_DEPLOYMENT_TARGET}" ;;
        aarch64-apple-ios-sim) sdk=iphonesimulator; triple="arm64-apple-ios${IPHONEOS_DEPLOYMENT_TARGET}-simulator" ;;
        *) echo "Unsupported iOS slice: $target" >&2; exit 1 ;;
    esac
    metal="$(dirname "$core_lib")/RCMetalRenderer.o"
    xcrun --sdk "$sdk" clang -target "$triple" -isysroot "$(xcrun --sdk "$sdk" --show-sdk-path)" \
        -fobjc-arc -O2 -I"$MEDIA_DIR/include" -c "$MEDIA_DIR/crates/rc-media-apple/src/RCMetalRenderer.m" -o "$metal"
    libtool -static -o "$merged" "$core_lib" "$MEDIA_DIR/target/$target/release/librc_media_apple.a" "$metal"
    libs+=("$merged")
done

rm -rf "$XCFW_DIR"
mkdir -p "$OUT_DIR"
args=()
for lib in "${libs[@]}"; do
    slice_id="$(basename "$(dirname "$(dirname "$lib")")")"
    hdr_dir="$(mktemp -d)/headers-$slice_id"
    mkdir -p "$hdr_dir"
    cp "$MEDIA_DIR/include/"*.h "$MEDIA_DIR/include/module.modulemap" "$hdr_dir/"
    args+=(-library "$lib" -headers "$hdr_dir")
done

echo "==> xcodebuild -create-xcframework"
xcodebuild -create-xcframework "${args[@]}" -output "$XCFW_DIR"
echo "==> 完成: $XCFW_DIR"
