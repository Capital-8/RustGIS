#!/usr/bin/env bash
# Builds a debug APK of the Android spike (#1902): target/android/rustgis-mobile.apk.
#
#   apps/rustgis-mobile/build-apk.sh [--debug] [abi ...]     (default: arm64-v8a x86_64)
#
# Needs: the Rust Android targets, cargo-ndk, and an Android SDK with the NDK, build-tools and a
# platform (see README.md). ANDROID_HOME names the SDK (default ~/Android/Sdk).
set -euo pipefail

profile=--release
if [ "${1:-}" = "--debug" ]; then profile=; shift; fi
abis=("$@")
[ ${#abis[@]} -gt 0 ] || abis=(arm64-v8a x86_64)

root=$(cd "$(dirname "$0")/../.." && pwd)
sdk=${ANDROID_HOME:-$HOME/Android/Sdk}
# The newest of each that is installed.
newest() { ls -d "$1"/*/ 2>/dev/null | sort -V | tail -1 | sed 's:/$::'; }
export ANDROID_NDK_HOME=${ANDROID_NDK_HOME:-$(newest "$sdk/ndk")}
tools=$(newest "$sdk/build-tools")
platform=$(newest "$sdk/platforms")
for need in "$ANDROID_NDK_HOME" "$tools" "$platform"; do
    [ -d "$need" ] || { echo "build-apk: the Android SDK at $sdk has no NDK, build-tools or platform (see README.md)" >&2; exit 1; }
done

out=$root/target/android
rm -rf "$out/apk" && mkdir -p "$out/apk"
targets=()
for abi in "${abis[@]}"; do targets+=(-t "$abi"); done
# API 26 (Android 8.0): the oldest this spike is linked for.
(cd "$root" && cargo ndk "${targets[@]}" -P 26 -o "$out/apk/lib" build $profile -p rustgis-mobile)
# Only the app's library goes in (cargo-ndk also copies a dependency's own cdylib), without its
# symbols: 115 MB with them, under 80 MB without.
find "$out/apk/lib" -name '*.so' ! -name librustgis_mobile.so -delete
strip=$(ls "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/bin/llvm-strip | head -1)
find "$out/apk/lib" -name librustgis_mobile.so -exec "$strip" {} \;

unsigned=$out/unsigned.apk
"$tools/aapt2" link -o "$unsigned" --manifest "$root/apps/rustgis-mobile/AndroidManifest.xml" -I "$platform/android.jar"
# The libraries go in uncompressed and page-aligned, so Android maps them straight from the APK.
(cd "$out/apk" && zip -q -0 -r "$unsigned" lib)
"$tools/zipalign" -f -p 16384 "$unsigned" "$out/aligned.apk" 2>/dev/null || "$tools/zipalign" -f -p 4 "$unsigned" "$out/aligned.apk"

# A debug key of this checkout's own: an APK must be signed to install, even on an emulator.
key=$out/debug.keystore
[ -f "$key" ] || keytool -genkeypair -keystore "$key" -storepass android -keypass android -alias debug \
    -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=RustGIS debug" >/dev/null 2>&1
"$tools/apksigner" sign --ks "$key" --ks-pass pass:android --key-pass pass:android --out "$out/rustgis-mobile.apk" "$out/aligned.apk"
rm -f "$unsigned" "$out/aligned.apk"
ls -lh "$out/rustgis-mobile.apk"
