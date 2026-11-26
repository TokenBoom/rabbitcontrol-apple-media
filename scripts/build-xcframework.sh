#!/usr/bin/env bash
# 构建 rc-mobile-ffi + rc-media-apple 静态库并打包为 RabbitControlCore.xcframework，
# 默认包含真机 (arm64) 与通用模拟器 (arm64 + x86_64 via lipo) 切片，
# 并生成 RabbitControlCore.xcframework.tar.gz 与标准 SHA256SUMS.txt。
# 本地与 CI 共用；core 依赖直接走 Cargo.toml 的 git+tag（如需本地联调提供 .cargo/config.toml）。
#
# 用法: ./scripts/build-xcframework.sh [输出目录]
# 环境变量:
#   RC_INCLUDE_X86_64_SIM=1|0  是否包含 x86_64-apple-ios 模拟器切片（默认 1）
#   RC_TARGETS                 显式覆写要构建的目标列表（优先级高于 RC_INCLUDE_X86_64_SIM）
#   RC_PACKAGE_ARCHIVE=1|0     是否生成 tar.gz 与 SHA256SUMS.txt（默认 1）
set -euo pipefail

MEDIA_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RC_INCLUDE_X86_64_SIM="${RC_INCLUDE_X86_64_SIM:-1}"
RC_PACKAGE_ARCHIVE="${RC_PACKAGE_ARCHIVE:-1}"
if [[ -n "${RC_TARGETS:-}" ]]; then
    TARGETS="$RC_TARGETS"
elif [[ "$RC_INCLUDE_X86_64_SIM" == "1" ]]; then
    TARGETS="aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios"
else
    TARGETS="aarch64-apple-ios aarch64-apple-ios-sim"
fi

OUT_DIR="${1:-$MEDIA_DIR/build}"
XCFW_DIR="$OUT_DIR/RabbitControlCore.xcframework"
ARCHIVE_NAME="RabbitControlCore.xcframework.tar.gz"
CHECKSUM_FILE="SHA256SUMS.txt"
# Keep native Cargo dependencies and the Metal object on the same minimum iOS version.
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-13.0}"

if ! command -v cargo >/dev/null 2>&1; then
    arch="$(uname -m)"
    [[ "$arch" == "arm64" ]] && arch="aarch64"
    TC="$HOME/.rustup/toolchains/stable-$arch-apple-darwin/bin"
    [[ -x "$TC/cargo" ]] && export PATH="$TC:$PATH"
fi

TMP_HDR_ROOT="$(mktemp -d)"
trap 'rm -rf "$TMP_HDR_ROOT"' EXIT

device_libs=()
sim_libs=()
for target in $TARGETS; do
    echo "==> cargo build --release --target $target"
    (cd "$MEDIA_DIR" && cargo build --release -p rc-mobile-ffi --target "$target")
    (cd "$MEDIA_DIR" && cargo build --release -p rc-media-apple --target "$target")
    core_lib="$MEDIA_DIR/target/$target/release/librc_mobile_ffi.a"
    merged="$(dirname "$core_lib")/librabbitcontrol_combined.a"
    case "$target" in
        aarch64-apple-ios)
            sdk=iphoneos
            triple="arm64-apple-ios${IPHONEOS_DEPLOYMENT_TARGET}"
            kind=device
            ;;
        aarch64-apple-ios-sim)
            sdk=iphonesimulator
            triple="arm64-apple-ios${IPHONEOS_DEPLOYMENT_TARGET}-simulator"
            kind=sim
            ;;
        x86_64-apple-ios)
            sdk=iphonesimulator
            triple="x86_64-apple-ios${IPHONEOS_DEPLOYMENT_TARGET}-simulator"
            kind=sim
            ;;
        *)
            echo "Unsupported iOS slice: $target" >&2
            exit 1
            ;;
    esac
    metal="$(dirname "$core_lib")/RCMetalRenderer.o"
    xcrun --sdk "$sdk" clang -target "$triple" -isysroot "$(xcrun --sdk "$sdk" --show-sdk-path)" \
        -fobjc-arc -O2 -I"$MEDIA_DIR/include" -c "$MEDIA_DIR/crates/rc-media-apple/src/RCMetalRenderer.m" -o "$metal"
    libtool -static -o "$merged" "$core_lib" "$MEDIA_DIR/target/$target/release/librc_media_apple.a" "$metal"
    if [[ "$kind" == "device" ]]; then
        device_libs+=("$merged")
    else
        sim_libs+=("$merged")
    fi
done

libs=()
if [[ "${#device_libs[@]}" -gt 0 ]]; then
    libs+=("${device_libs[@]}")
fi
if [[ "${#sim_libs[@]}" -eq 1 ]]; then
    libs+=("${sim_libs[0]}")
elif [[ "${#sim_libs[@]}" -gt 1 ]]; then
    universal_sim_dir="$MEDIA_DIR/target/ios-sim-universal/release"
    mkdir -p "$universal_sim_dir"
    universal_sim_lib="$universal_sim_dir/librabbitcontrol_combined.a"
    echo "==> lipo -create universal simulator static lib (${sim_libs[*]})"
    lipo -create "${sim_libs[@]}" -output "$universal_sim_lib"
    libs+=("$universal_sim_lib")
fi

if [[ "${#libs[@]}" -eq 0 ]]; then
    echo "No iOS slices were built (TARGETS=$TARGETS)" >&2
    exit 1
fi

rm -rf "$XCFW_DIR"
mkdir -p "$OUT_DIR"
args=()
for lib in "${libs[@]}"; do
    slice_id="$(basename "$(dirname "$(dirname "$lib")")")"
    hdr_dir="$TMP_HDR_ROOT/headers-$slice_id"
    mkdir -p "$hdr_dir"
    cp "$MEDIA_DIR/include/"*.h "$MEDIA_DIR/include/module.modulemap" "$hdr_dir/"
    args+=(-library "$lib" -headers "$hdr_dir")
done

echo "==> xcodebuild -create-xcframework"
xcodebuild -create-xcframework "${args[@]}" -output "$XCFW_DIR"
echo "==> 完成 xcframework: $XCFW_DIR"

if [[ "$RC_PACKAGE_ARCHIVE" == "1" ]]; then
    echo "==> 打包 $ARCHIVE_NAME 并生成 $CHECKSUM_FILE"
    (
        cd "$OUT_DIR"
        rm -f "$ARCHIVE_NAME" "$CHECKSUM_FILE"
        tar -czf "$ARCHIVE_NAME" RabbitControlCore.xcframework
        shasum -a 256 "$ARCHIVE_NAME" > "$CHECKSUM_FILE"
    )
    echo "==> 完成归档与校验文件: $OUT_DIR/$ARCHIVE_NAME, $OUT_DIR/$CHECKSUM_FILE"
fi
