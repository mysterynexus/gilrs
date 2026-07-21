package org.gilrs;

import android.app.Activity;
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
public final class GilrsHook implements InvocationHandler, Runnable {
    /**
     * Bounds the proxy chain. Other libraries (appcompat, for one) also replace the window
     * callback; if one of them keeps doing so we would otherwise wrap it again on every poll and
     * grow the delegation chain until it overflows the stack.
     */
    private static final int MAX_INSTALLS = 4;

    private static GilrsHook instance;

    private final Activity activity;
    private volatile Object delegate;
    private volatile Object proxy;
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
     * Installs the hook, or reinstalls it if something else has since replaced the window
     * callback. Safe to call repeatedly from any thread.
     */
    public static void ensureInstalled(Activity activity) {
        GilrsHook hook;
        synchronized (GilrsHook.class) {
            // Android destroys and recreates activities while the process lives on, so the
            // activity handed to us here may be a different one than last time. Starting over
            // resets the install count and the stale proxy along with it.
            if (instance == null || instance.activity != activity) {
                instance = new GilrsHook(activity);
            }
            hook = instance;
        }
        // Checked here rather than unconditionally posting, so that the steady state costs
        // nothing beyond two field reads.
        if (hook.needsInstall()) {
            activity.runOnUiThread(hook);
        }
    }

    private boolean needsInstall() {
        synchronized (GilrsHook.class) {
            if (installs >= MAX_INSTALLS) {
                return false;
            }
        }
        Window window = activity.getWindow();
        return window != null && window.getCallback() != proxy;
    }

    @Override
    public void run() {
        synchronized (GilrsHook.class) {
            if (installs >= MAX_INSTALLS) {
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
            proxy = Proxy.newProxyInstance(
                    GilrsHook.class.getClassLoader(),
                    new Class<?>[] { Window.Callback.class },
                    this);
            installs++;
            window.setCallback((Window.Callback) proxy);
        }
    }

    @Override
    public Object invoke(Object unused, Method method, Object[] args) throws Throwable {
        boolean outermost = depth == 0;
        depth++;
        try {
            if (outermost && args != null && args.length == 1) {
                String name = method.getName();
                if (name.equals("dispatchKeyEvent") && args[0] instanceof KeyEvent) {
                    nativeKeyEvent((KeyEvent) args[0]);
                } else if (name.equals("dispatchGenericMotionEvent")
                        && args[0] instanceof MotionEvent) {
                    nativeMotionEvent((MotionEvent) args[0]);
                }
            }
            return method.invoke(delegate, args);
        } catch (InvocationTargetException e) {
            throw e.getCause();
        } finally {
            depth--;
        }
    }

    private static native void nativeKeyEvent(KeyEvent event);

    private static native void nativeMotionEvent(MotionEvent event);
}
