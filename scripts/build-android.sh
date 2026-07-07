#!/usr/bin/env bash
# Cross-compiles the Rust crates that ship in the Android APK (`arc-adb`, and
# later the JNI `.so`) to android/arm64.
#
# Why the ceremony: `arc-adb` pulls aws-lc-sys (SPAKE2 FFI + rustls's aws-lc-rs),
# whose cmake build does NOT self-configure for Android — it sets
# CMAKE_SYSTEM_NAME=Android but never tells cmake where the NDK is, and exposes
# no way to inject cmake `-D` flags. So we (a) point bindgen at the NDK sysroot
# and (b) wrap cmake with a shim that injects CMAKE_ANDROID_NDK/ABI/API. With
# those, the whole aws-lc + rustls + rsa + rcgen stack cross-compiles cleanly.
#
# Usage: scripts/build-android.sh [ABI] [API] [-- extra cargo-ndk args]
#   ABI defaults to arm64-v8a, API to 24.
set -euo pipefail

ABI="${1:-arm64-v8a}"
API="${2:-24}"
case "$ABI" in
  arm64-v8a) TRIPLE=aarch64-linux-android ;;
  armeabi-v7a) TRIPLE=armv7a-linux-androideabi ;;
  x86_64) TRIPLE=x86_64-linux-android ;;
  *) echo "unsupported ABI: $ABI" >&2; exit 1 ;;
esac

# Locate the NDK.
NDK="${ANDROID_NDK_HOME:-}"
if [[ -z "$NDK" ]]; then
  NDK=$(ls -d "$HOME/Library/Android/sdk/ndk/"* 2>/dev/null | sort -V | tail -1 || true)
fi
[[ -d "$NDK" ]] || { echo "Android NDK not found (set ANDROID_NDK_HOME)" >&2; exit 1; }

# Locate an SDK cmake bin dir that ships ninja alongside cmake.
CMAKE_BIN=""
for d in $(ls -d "$HOME/Library/Android/sdk/cmake/"*/bin 2>/dev/null | sort -V -r); do
  [[ -x "$d/cmake" && -x "$d/ninja" ]] && { CMAKE_BIN="$d"; break; }
done
[[ -n "$CMAKE_BIN" ]] || { echo "Android SDK cmake (with ninja) not found" >&2; exit 1; }

HOST_TAG=$(ls "$NDK/toolchains/llvm/prebuilt/" | head -1)
SYSROOT="$NDK/toolchains/llvm/prebuilt/$HOST_TAG/sysroot"

# cmake shim: inject the Android NDK config aws-lc-sys omits, on configure only.
SHIM=$(mktemp -d)/cmake
cat > "$SHIM" <<EOF
#!/bin/sh
for a in "\$@"; do case "\$a" in --build|--install) exec "$CMAKE_BIN/cmake" "\$@";; esac; done
exec "$CMAKE_BIN/cmake" "\$@" \\
  -DCMAKE_ANDROID_NDK="$NDK" \\
  -DCMAKE_ANDROID_ARCH_ABI="$ABI" \\
  -DCMAKE_SYSTEM_VERSION="$API" \\
  -DCMAKE_MAKE_PROGRAM="$CMAKE_BIN/ninja"
EOF
chmod +x "$SHIM"

export BINDGEN_EXTRA_CLANG_ARGS="--sysroot=$SYSROOT -target ${TRIPLE}${API}"
export CMAKE="$SHIM"
export CMAKE_GENERATOR="Ninja"
export PATH="$CMAKE_BIN:$PATH"

shift 2 2>/dev/null || shift $#
[[ "${1:-}" == "--" ]] && shift || true

echo "→ cargo ndk -t $ABI -p $API build -p arc-adb (NDK=$NDK)"
exec cargo ndk -t "$ABI" -p "$API" build -p arc-adb "$@"
