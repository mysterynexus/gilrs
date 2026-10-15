package android.app;

import android.content.Context;
import android.view.Window;
import java.util.ArrayDeque;
import java.util.Queue;

public class Activity extends Context {
    private final Application application;
    private final Window window;
    private final Queue<Runnable> uiTasks = new ArrayDeque<>();
    private boolean runImmediately = true;
    private boolean destroyed;

    public Activity(Application application, Window.Callback callback) {
        this.application = application;
        this.window = new Window(callback);
    }

    @Override
    public Context getApplicationContext() {
        return application;
    }

    public Application getApplication() {
        return application;
    }

    public Window getWindow() {
        return window;
    }

    public void runOnUiThread(Runnable task) {
        if (runImmediately) {
            task.run();
        } else {
            uiTasks.add(task);
        }
    }

    public boolean isDestroyed() {
        return destroyed;
    }

    public void destroy() {
        destroyed = true;
    }

    public void queueUiTasks() {
        runImmediately = false;
    }

    public int queuedUiTasks() {
        return uiTasks.size();
    }

    public void drainUiTasks() {
        while (!uiTasks.isEmpty()) {
            uiTasks.remove().run();
        }
    }
}
