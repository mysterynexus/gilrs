package org.gilrs;

import android.app.Activity;
import android.app.Application;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.Window;

public final class GilrsHookLifecycleTest {
    public static void main(String[] args) {
        System.load(args[1]);
        switch (args[0]) {
            case "application": application(); break;
            case "legacy": legacy(); break;
            case "recreation": recreation(); break;
            case "queued_destroy": queuedDestroy(); break;
            case "queued_replace": queuedReplace(); break;
            case "rewrap": rewrap(); break;
            case "cleanup": cleanup(); break;
            default: throw new AssertionError("Unknown scenario: " + args[0]);
        }
        System.out.println("PASS " + args[0]);
    }

    private static void application() {
        Application app = new Application();
        GilrsHook.initialize(app);
        check(app.registrations() == 1,
                "Application callbacks must be registered before initialize returns");
        GilrsHook.initialize(app);
        GilrsHook.ensureInstalled();
        check(app.registrations() == 1, "Repeated initialization registered callbacks again");

        RecordingCallback original = new RecordingCallback();
        Activity activity = new Activity(app, original);
        app.resume(activity);
        check(activity.getWindow().getCallback() != original, "First resume did not hook window");
        dispatch(activity.getWindow().getCallback(), original);
        check(keyEvents() == 1 && motionEvents() == 1, "First resume did not forward input");
    }

    private static void legacy() {
        Application app = new Application();
        RecordingCallback original = new RecordingCallback();
        Activity activity = new Activity(app, original);
        GilrsHook.initialize(activity);
        Window.Callback installed = activity.getWindow().getCallback();
        check(installed != original, "Legacy Activity context did not install immediately");
        GilrsHook.initialize(activity);
        GilrsHook.ensureInstalled();
        check(activity.getWindow().getCallback() == installed,
                "Repeated initialization replaced an already installed hook");
        check(app.registrations() == 1, "Legacy initialization registered callbacks again");
        dispatch(installed, original);
        check(keyEvents() == 1 && motionEvents() == 1, "Legacy hook did not forward input");
    }

    private static void recreation() {
        Application app = new Application();
        GilrsHook.initialize(app);
        RecordingCallback firstOriginal = new RecordingCallback();
        Activity first = new Activity(app, firstOriginal);
        app.resume(first);
        Window.Callback retiredProxy = first.getWindow().getCallback();
        app.destroy(first);
        check(first.getWindow().getCallback() == firstOriginal,
                "Destroyed Activity retained its owned callback hook");

        RecordingCallback nextOriginal = new RecordingCallback();
        Activity next = new Activity(app, nextOriginal);
        app.resume(next);
        dispatch(next.getWindow().getCallback(), nextOriginal);
        check(keyEvents() == 1 && motionEvents() == 1, "Recreated Activity did not forward input");
        dispatch(retiredProxy, firstOriginal);
        check(keyEvents() == 1 && motionEvents() == 1,
                "Retired callback still forwarded native input");
        app.destroy(first);
        GilrsHook.ensureInstalled();
        dispatch(next.getWindow().getCallback(), nextOriginal);
        check(keyEvents() == 2 && motionEvents() == 2,
                "Destroy notification for an older Activity disabled the current hook");
    }

    private static void queuedDestroy() {
        Application app = new Application();
        GilrsHook.initialize(app);
        RecordingCallback original = new RecordingCallback();
        Activity activity = new Activity(app, original);
        activity.queueUiTasks();
        app.resume(activity);
        check(activity.queuedUiTasks() > 0, "Test did not queue installation");
        GilrsHook.ensureInstalled();
        GilrsHook.ensureInstalled();
        check(activity.queuedUiTasks() == 1, "Polling accumulated duplicate UI tasks");
        app.destroy(activity);
        activity.drainUiTasks();
        check(activity.getWindow().getCallback() == original,
                "Pending UI task installed a hook on a destroyed Activity");
        GilrsHook.ensureInstalled();
        activity.drainUiTasks();
        check(activity.getWindow().getCallback() == original,
                "Polling revived a destroyed Activity hook");
    }

    private static void queuedReplace() {
        Application app = new Application();
        GilrsHook.initialize(app);
        RecordingCallback firstOriginal = new RecordingCallback();
        Activity first = new Activity(app, firstOriginal);
        first.queueUiTasks();
        app.resume(first);
        check(first.queuedUiTasks() > 0, "Test did not queue first installation");

        RecordingCallback nextOriginal = new RecordingCallback();
        Activity next = new Activity(app, nextOriginal);
        app.resume(next);
        first.drainUiTasks();
        check(first.getWindow().getCallback() == firstOriginal,
                "Stale UI task installed after another Activity became current");
        dispatch(next.getWindow().getCallback(), nextOriginal);
        check(keyEvents() == 1 && motionEvents() == 1,
                "Replacing an Activity lost the current event hook");
    }

    private static void rewrap() {
        Application app = new Application();
        RecordingCallback original = new RecordingCallback();
        Activity activity = new Activity(app, original);
        GilrsHook.initialize(activity);
        ForwardingCallback external = new ForwardingCallback(activity.getWindow().getCallback());
        activity.getWindow().setCallback(external);
        GilrsHook.ensureInstalled();
        Window.Callback reinstalled = activity.getWindow().getCallback();
        check(reinstalled != external, "Hook did not reinstall after external wrapper");
        GilrsHook.ensureInstalled();
        check(activity.getWindow().getCallback() == reinstalled,
                "Polling changed a hook that was already installed");
        dispatch(reinstalled, original);
        check(external.keys == 1 && external.motions == 1,
                "Reinstalled hook failed to delegate through external wrapper once");
        check(keyEvents() == 1 && motionEvents() == 1,
                "Rewrapped hook forwarded the same event more than once");
    }

    private static void cleanup() {
        Application app = new Application();
        RecordingCallback original = new RecordingCallback();
        Activity activity = new Activity(app, original);
        GilrsHook.initialize(activity);
        ForwardingCallback external = new ForwardingCallback(activity.getWindow().getCallback());
        activity.getWindow().setCallback(external);
        app.destroy(activity);
        check(activity.getWindow().getCallback() == external,
                "Cleanup overwrote another library's callback");
        dispatch(external, original);
        check(keyEvents() == 0 && motionEvents() == 0,
                "Destroyed hook retained inside another wrapper still forwarded native input");
    }

    private static void dispatch(Window.Callback callback, RecordingCallback original) {
        int priorKeys = original.keys;
        int priorMotions = original.motions;
        KeyEvent key = new KeyEvent();
        MotionEvent motion = new MotionEvent();
        check(callback.dispatchKeyEvent(key), "Key callback result changed");
        check(!callback.dispatchGenericMotionEvent(motion), "Motion callback result changed");
        check(original.keys == priorKeys + 1 && original.motions == priorMotions + 1,
                "Original callback did not receive each event exactly once");
        check(original.lastKey == key && original.lastMotion == motion,
                "Hook replaced the original event objects");
        check("original".equals(callback.marker()), "Non-input method did not delegate");
    }

    private static void check(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    private static native int keyEvents();
    private static native int motionEvents();

    public static final class RecordingCallback implements Window.Callback {
        int keys;
        int motions;
        KeyEvent lastKey;
        MotionEvent lastMotion;

        @Override
        public boolean dispatchKeyEvent(KeyEvent event) {
            keys++;
            lastKey = event;
            return true;
        }

        @Override
        public boolean dispatchGenericMotionEvent(MotionEvent event) {
            motions++;
            lastMotion = event;
            return false;
        }

        @Override
        public String marker() {
            return "original";
        }
    }

    public static final class ForwardingCallback implements Window.Callback {
        private final Window.Callback delegate;
        int keys;
        int motions;

        ForwardingCallback(Window.Callback delegate) {
            this.delegate = delegate;
        }

        @Override
        public boolean dispatchKeyEvent(KeyEvent event) {
            keys++;
            return delegate.dispatchKeyEvent(event);
        }

        @Override
        public boolean dispatchGenericMotionEvent(MotionEvent event) {
            motions++;
            return delegate.dispatchGenericMotionEvent(event);
        }

        @Override
        public String marker() {
            return delegate.marker();
        }
    }
}
