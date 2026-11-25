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
    libtool -static -o "$merged" "$core_lib" "$MEDIA_DIR/target/$target/release/librc_media_apple.a"
    libs+=("$merged")
done

rm -rf "$XCFW_DIR"
mkdir -p "$OUT_DIR"
args=()
for lib in "${libs[@]}"; do
    slice_id="$(basename "$(dirname "$(dirname "$lib")")")"
    hdr_dir="$(mktemp -d)/headers-$slice_id"
    mkdir -p "$hdr_dir"
    cp "$MEDIA_DIR/include/rabbitcontrol_core.h" "$MEDIA_DIR/include/module.modulemap" "$hdr_dir/"
    args+=(-library "$lib" -headers "$hdr_dir")
done

echo "==> xcodebuild -create-xcframework"
xcodebuild -create-xcframework "${args[@]}" -output "$XCFW_DIR"
echo "==> 完成: $XCFW_DIR"
