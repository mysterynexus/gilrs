package android.view;

public final class Window {
    public interface Callback {
        boolean dispatchKeyEvent(KeyEvent event);
        boolean dispatchGenericMotionEvent(MotionEvent event);
        String marker();
    }

    private Callback callback;

    public Window(Callback callback) {
        this.callback = callback;
    }

    public Callback getCallback() {
        return callback;
    }

    public void setCallback(Callback callback) {
        this.callback = callback;
    }
}
