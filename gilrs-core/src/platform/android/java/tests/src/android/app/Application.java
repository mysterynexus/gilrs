package android.app;

import android.content.Context;
import android.os.Bundle;
import java.util.ArrayList;
import java.util.List;

public final class Application extends Context {
    public interface ActivityLifecycleCallbacks {
        void onActivityCreated(Activity activity, Bundle state);
        void onActivityStarted(Activity activity);
        void onActivityResumed(Activity activity);
        void onActivityPaused(Activity activity);
        void onActivityStopped(Activity activity);
        void onActivitySaveInstanceState(Activity activity, Bundle state);
        void onActivityDestroyed(Activity activity);
    }

    private final List<ActivityLifecycleCallbacks> callbacks = new ArrayList<>();
    private int registrations;

    @Override
    public Context getApplicationContext() {
        return this;
    }

    public void registerActivityLifecycleCallbacks(ActivityLifecycleCallbacks callback) {
        registrations++;
        callbacks.add(callback);
    }

    public int registrations() {
        return registrations;
    }

    public void resume(Activity activity) {
        for (ActivityLifecycleCallbacks callback : new ArrayList<>(callbacks)) {
            callback.onActivityResumed(activity);
        }
    }

    public void destroy(Activity activity) {
        activity.destroy();
        for (ActivityLifecycleCallbacks callback : new ArrayList<>(callbacks)) {
            callback.onActivityDestroyed(activity);
        }
    }
}
