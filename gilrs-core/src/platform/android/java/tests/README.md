Run `./run.sh` with a JDK (11 or newer) and a C compiler. Set `GILRS_TEST_JDK` to
select a JDK. The runner compiles the actual `../GilrsHook.java`; there are no test
or runtime dependencies to download.

The fake Android API controls lifecycle delivery and the UI task queue. A tiny
JNI library counts forwarded events, including when another library wraps the
hook. These tests cover helper logic; an Android device is still needed to check
real framework lifecycle ordering and controller input delivery.
