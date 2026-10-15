package org.gilrs;

import android.app.Activity;
import android.app.Application;
import android.app.NativeActivity;
import android.content.Context;
import android.os.Bundle;
import android.util.Log;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.Window;

import java.lang.reflect.InvocationHandler;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;

/**
 * Sniffs gamepad input by wrapping the activity's Window.Callback.
 *
 * Android exposes no way to poll gamepad state; button and axis data only exist as KeyEvent and
 * MotionEvent objects dispatched to the focused window. Under GameActivity (what winit and bevy
 * use) every such event travels DecorView.dispatchKeyEvent -> Activity.dispatchKeyEvent ->
 * SurfaceView, and Activity.dispatchKeyEvent *is* Window.Callback. Wrapping that callback puts us
 * ahead of the consumer without competing with it: we observe and always delegate.
 *
 * A reflection Proxy is used rather than implementing Window.Callback directly so that every
 * method is delegated generically. A hand-written implementation would throw AbstractMethodError
 * as soon as a future API level adds a method to the interface.
 */
public final class GilrsHook implements Runnable {
    /**
     * Bounds the proxy chain. Other libraries (appcompat, for one) also replace the window
     * callback; if one of them keeps doing so we would otherwise wrap it again on every poll and
     * grow the delegation chain until it overflows the stack.
     */
    private static final int MAX_INSTALLS = 4;

    private static GilrsHook instance;
    private static Application application;

    private static final Application.ActivityLifecycleCallbacks lifecycleCallbacks =
            new Application.ActivityLifecycleCallbacks() {
                @Override
                public void onActivityCreated(Activity activity, Bundle state) {}

                @Override
                public void onActivityStarted(Activity activity) {}

                @Override
                public void onActivityResumed(Activity activity) {
                    useActivity(activity);
                }

                @Override
                public void onActivityPaused(Activity activity) {}

                @Override
                public void onActivityStopped(Activity activity) {}

                @Override
                public void onActivitySaveInstanceState(Activity activity, Bundle state) {}

                @Override
                public void onActivityDestroyed(Activity activity) {
                    GilrsHook hook;
                    synchronized (GilrsHook.class) {
                        if (instance == null || instance.activity != activity) {
                            return;
                        }
                        hook = instance;
                        hook.active = false;
                        instance = null;
                    }
                    hook.restoreCallback();
                }
            };

    private final Activity activity;
    private Window.Callback delegate;
    private Window.Callback proxy;
    private volatile boolean active = true;
    private boolean installQueued;
    private int installs;
    /**
     * Re-entrancy depth. If we end up wrapped around an older proxy of our own, the event passes
     * through this handler twice; only the outermost pass should report it.
     */
    private int depth;

    private GilrsHook(Activity activity) {
        this.activity = activity;
    }

    /**
     * Registers lifecycle tracking synchronously, before the native event loop can let the
     * activity resume. UI work is posted without waiting: GameActivity may still be waiting for
     * that event loop on its UI thread. An Activity context also supports legacy initialization
     * after onResume, since lifecycle callbacks do not replay earlier events.
     */
    public static void initialize(Context context) {
        Context applicationContext = context.getApplicationContext();
        if (!(applicationContext instanceof Application)) {
            throw new IllegalArgumentException("gilrs requires an Android Application context");
        }
        synchronized (GilrsHook.class) {
            if (application == null) {
                application = (Application) applicationContext;
                application.registerActivityLifecycleCallbacks(lifecycleCallbacks);
            }
            // A resume callback may already have selected a newer activity after registration.
            if (instance == null && context instanceof Activity) {
                instance = new GilrsHook((Activity) context);
                warnIfNativeActivity((Activity) context);
            }
        }
        ensureInstalled();
    }

    private static void useActivity(Activity activity) {
        GilrsHook previous = null;
        synchronized (GilrsHook.class) {
            if (instance == null || instance.activity != activity) {
                previous = instance;
                if (previous != null) {
                    previous.active = false;
                }
                instance = new GilrsHook(activity);
                warnIfNativeActivity(activity);
            }
        }
        // Lifecycle callbacks run on the UI thread, so both windows can be touched directly.
        if (previous != null) {
            previous.restoreCallback();
        }
        ensureInstalled();
    }

    private static void warnIfNativeActivity(Activity activity) {
        if (activity instanceof NativeActivity) {
            Log.w("gilrs", "NativeActivity consumes input before the window callback. Gamepads "
                    + "require explicit forwarding through gilrs::android::handle_input_event "
                    + "(the android-activity feature), or GameActivity.");
        }
    }

    /** Checks/reinstalls the current activity's hook. Safe to call repeatedly from any thread. */
    public static void ensureInstalled() {
        GilrsHook hook;
        synchronized (GilrsHook.class) {
            hook = instance;
            if (hook == null || hook.installQueued || hook.installs >= MAX_INSTALLS) {
                return;
            }
            hook.installQueued = true;
        }
        // Keep all window access on the UI thread and avoid accumulating poll requests there.
        hook.activity.runOnUiThread(hook);
    }

    @Override
    public void run() {
        synchronized (GilrsHook.class) {
            installQueued = false;
            // Queued checks can outlive either destruction or selection of another activity.
            if (instance != this || !active || installs >= MAX_INSTALLS) {
                return;
            }
            if (activity.isDestroyed()) {
                active = false;
                instance = null;
                restoreCallback();
                return;
            }
            Window window = activity.getWindow();
            if (window == null) {
                return;
            }
            Window.Callback current = window.getCallback();
            if (current == proxy || current == null) {
                return;
            }
            delegate = current;
            proxy = (Window.Callback) Proxy.newProxyInstance(
                    GilrsHook.class.getClassLoader(),
                    new Class<?>[] { Window.Callback.class },
                    new CallbackHandler(this, current));
            installs++;
            window.setCallback(proxy);
        }
    }

    private void restoreCallback() {
        Window window = activity.getWindow();
        if (window != null && proxy != null && window.getCallback() == proxy) {
            window.setCallback(delegate);
        }
    }

    private static final class CallbackHandler implements InvocationHandler {
        private final GilrsHook hook;
        private final Window.Callback delegate;

        private CallbackHandler(GilrsHook hook, Window.Callback delegate) {
            this.hook = hook;
            this.delegate = delegate;
        }

        @Override
        public Object invoke(Object unused, Method method, Object[] args) throws Throwable {
            boolean outermost = hook.depth == 0;
            hook.depth++;
            try {
                if (hook.active && outermost && args != null && args.length == 1) {
                    String name = method.getName();
                    if (name.equals("dispatchKeyEvent") && args[0] instanceof KeyEvent) {
                        nativeKeyEvent((KeyEvent) args[0]);
                    } else if (name.equals("dispatchGenericMotionEvent")
                            && args[0] instanceof MotionEvent) {
                        nativeMotionEvent((MotionEvent) args[0]);
                    }
                }
                // Capture each proxy's delegate separately. A later wrapper may still delegate
                // to an older proxy, and updating its delegate would create a callback cycle.
                return method.invoke(delegate, args);
            } catch (InvocationTargetException e) {
                throw e.getCause();
            } finally {
                hook.depth--;
            }
        }
    }

    private static native void nativeKeyEvent(KeyEvent event);

    private static native void nativeMotionEvent(MotionEvent event);
}
