#!/bin/sh
# Runs the real Java hook against a small, deterministic Android lifecycle model.
set -eu

cd "$(dirname "$0")/.."
test_jdk="${GILRS_TEST_JDK:-}"
if [ -z "$test_jdk" ] && [ -x /usr/libexec/java_home ]; then
    test_jdk="$(/usr/libexec/java_home)"
fi
if [ -n "$test_jdk" ]; then
    test_java="$test_jdk/bin/java"
    test_javac="$test_jdk/bin/javac"
else
    test_javac="$(command -v javac)"
    test_javac="$(readlink -f "$test_javac")"
    test_jdk="$(dirname "$(dirname "$test_javac")")"
    test_java="$test_jdk/bin/java"
fi

out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT HUP INT TERM
mkdir "$out/classes"
find tests/src -name '*.java' -print > "$out/sources"
printf '%s\n' GilrsHook.java >> "$out/sources"
"$test_javac" --release 8 -d "$out/classes" @"$out/sources"

case "$(uname -s)" in
    Darwin) jni_platform=darwin; library="$out/libgilrs_hook_test.dylib" ;;
    Linux) jni_platform=linux; library="$out/libgilrs_hook_test.so" ;;
    *) echo 'This test runner currently supports macOS and Linux.' >&2; exit 1 ;;
esac
"${CC:-cc}" -shared -fPIC -Wall -Wextra -Werror \
    -I"$test_jdk/include" -I"$test_jdk/include/$jni_platform" \
    tests/native_recorder.c -o "$library"

# Each case gets a fresh JVM, just as the static helper starts fresh in a new process.
for scenario in application legacy recreation queued_destroy queued_replace rewrap cleanup; do
    "$test_java" -cp "$out/classes" org.gilrs.GilrsHookLifecycleTest "$scenario" "$library"
done
