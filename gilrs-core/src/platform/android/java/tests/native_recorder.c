#include <jni.h>

static jint key_events;
static jint motion_events;

JNIEXPORT void JNICALL Java_org_gilrs_GilrsHook_nativeKeyEvent(
    JNIEnv *env, jclass cls, jobject event) {
    (void)env;
    (void)cls;
    (void)event;
    key_events++;
}

JNIEXPORT void JNICALL Java_org_gilrs_GilrsHook_nativeMotionEvent(
    JNIEnv *env, jclass cls, jobject event) {
    (void)env;
    (void)cls;
    (void)event;
    motion_events++;
}

JNIEXPORT jint JNICALL Java_org_gilrs_GilrsHookLifecycleTest_keyEvents(
    JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    return key_events;
}

JNIEXPORT jint JNICALL Java_org_gilrs_GilrsHookLifecycleTest_motionEvents(
    JNIEnv *env, jclass cls) {
    (void)env;
    (void)cls;
    return motion_events;
}
