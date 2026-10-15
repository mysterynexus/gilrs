package android.app;

import android.view.Window;

public final class NativeActivity extends Activity {
    public NativeActivity(Application application, Window.Callback callback) {
        super(application, callback);
    }
}
