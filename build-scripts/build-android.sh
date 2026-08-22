#!/bin/bash
#
# Build libntsync_android.so for Android aarch64 and x86_64.
# Mirrors the style of the proton-wine arm64ec build script.
# Always links with 16KB page-size alignment (see .cargo/config.toml).
#
# Usage:
#   ./build-scripts/build-android.sh [--build] [--install] [--clean]
#
# Environment overrides:
#   NDK        Android NDK path (default: $HOME/Android/Sdk/ndk/27.3.13750724)
#   API        Android API level (default: 28)
#   OUTPUT_DIR Install destination (default: $HOME/compiled-files)

set -e

export OUTPUT_DIR="$HOME/compiled-files"
export NDK="${NDK:-$HOME/Android/Sdk/ndk/27.3.13750724}"
export API="${API:-28}"

export deps="$HOME/termuxfs/aarch64/data/data/com.termux/files/usr"
export RUNTIME_PATH="/data/data/com.termux/files/usr"

export TOOLCHAIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"

# triple:clang-prefix:abi
TARGETS=(
  "aarch64-linux-android:aarch64-linux-android:arm64-v8a"
  "x86_64-linux-android:x86_64-linux-android:x86_64"
)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

build_one() {
  local triple="$1" clang="$2" abi="$3"
  local target="$clang$API"
  local var="CARGO_TARGET_$(echo "$triple" | tr 'a-z-' 'A-Z_')_LINKER"

  echo "Building libntsync_android.so for $target ..."
  if ! rustup target list --installed | grep -q "$triple"; then
    rustup target add "$triple"
  fi
  env "$var=$TOOLCHAIN/$target-clang" \
      AR="$TOOLCHAIN/llvm-ar" \
      cargo build --release --target "$triple" --manifest-path "$PROJECT_ROOT/Cargo.toml"

  local so="$PROJECT_ROOT/target/$triple/release/libntsync_android.so"
  echo "Verifying 16KB page-size alignment..."
  readelf -lW "$so" | grep LOAD
  if ! readelf -lW "$so" | grep LOAD | awk '{print $NF}' | grep -qx "0x4000"; then
    echo "ERROR: LOAD segments are not 16KB-aligned!"
    exit 1
  fi
  echo "Exported symbols:"
  nm -D "$so" | grep " T ntsync"
  echo "Build OK ($abi): $so"
}

for arg in "$@"
do
  if [ "$arg" == "--clean" ];
  then
    echo "Cleaning..."
    for t in "${TARGETS[@]}"; do
      cargo clean --manifest-path "$PROJECT_ROOT/Cargo.toml" --target "${t%%:*}" || true
    done
  fi

  if [ "$arg" == "--build" ];
  then
    for t in "${TARGETS[@]}"; do
      IFS=':' read -r triple clang abi <<< "$t"
      build_one "$triple" "$clang" "$abi"
    done
  fi

  if [ "$arg" == "--install" ]
  then
    echo "Installing..."
    mkdir -p "$OUTPUT_DIR/include"
    cp "$PROJECT_ROOT/include/ntsync_user.h" "$OUTPUT_DIR/include/"
    for t in "${TARGETS[@]}"; do
      IFS=':' read -r triple clang abi <<< "$t"
      mkdir -p "$OUTPUT_DIR/lib/$abi"
      cp "$PROJECT_ROOT/target/$triple/release/libntsync_android.so" "$OUTPUT_DIR/lib/$abi/"
      echo "Installed -> $OUTPUT_DIR/lib/$abi/libntsync_android.so"
    done
    echo "Installed ntsync_user.h -> $OUTPUT_DIR/include/"
    echo "Copy the per-ABI .so into \$RUNTIME_PATH/lib or your Wine libdir so the"
    echo "dynamic loader finds it (rpath=\$RUNTIME_PATH/lib)."
  fi
done
