#!/bin/sh
# Regenerates ../gilrs_hook.dex from GilrsHook.java.
#
# Only needs to be run when GilrsHook.java changes; the resulting dex is committed so that building
# gilrs requires no JDK or Android SDK.
#
# Requires a JDK and the Android SDK. Override with ANDROID_HOME / ANDROID_PLATFORM / BUILD_TOOLS.
set -eu

ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
ANDROID_PLATFORM="${ANDROID_PLATFORM:-android-34}"
BUILD_TOOLS="${BUILD_TOOLS:-34.0.0}"

cd "$(dirname "$0")"
out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT

javac -source 8 -target 8 -nowarn \
    -bootclasspath "$ANDROID_HOME/platforms/$ANDROID_PLATFORM/android.jar" \
    -d "$out" GilrsHook.java

"$ANDROID_HOME/build-tools/$BUILD_TOOLS/d8" \
    --release \
    --min-api 26 \
    --lib "$ANDROID_HOME/platforms/$ANDROID_PLATFORM/android.jar" \
    --output "$out" \
    "$out/org/gilrs/GilrsHook.class"

cp "$out/classes.dex" ../gilrs_hook.dex
echo "wrote ../gilrs_hook.dex ($(wc -c < ../gilrs_hook.dex | tr -d ' ') bytes)"
