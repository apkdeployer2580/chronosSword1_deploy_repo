package kr.chronos.sword;

import android.content.Intent;
import android.os.Build;
import android.os.Bundle;
import android.view.View;
import android.view.Window;
import android.view.WindowManager;
import android.view.WindowInsets;
import android.view.WindowInsetsController;

import com.google.androidgamesdk.GameActivity;

/**
 * Thin GameActivity subclass. Everything else lives in the Rust cdylib
 * (libmain.so), entered via android_main(). The static loadLibrary and the
 * manifest's android.app.lib_name meta-data both name "main".
 */
public class MainActivity extends GameActivity {
    static {
        System.loadLibrary("main");
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        // Before super: that is where GameActivity starts android_main, and the
        // native side's first act may be a notice about the save it applied.
        CsSave.attach(this);
        super.onCreate(savedInstanceState);
        // The game is played with long stretches of reading and no touches, so
        // without this the display times out mid-session. FLAG_KEEP_SCREEN_ON
        // rather than a wake lock: it needs no permission and is scoped to this
        // window, so it cannot outlive the app.
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        hideSystemBars();
    }

    /** The 세이브 내보내기 / 불러오기 document screens answer here. */
    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        if (!CsSave.onActivityResult(this, requestCode, resultCode, data)) {
            super.onActivityResult(requestCode, resultCode, data);
        }
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        // The legacy visibility flags are cleared by the system whenever the
        // bars are shown, so re-assert on every focus gain rather than only at
        // create time.
        if (hasFocus) {
            hideSystemBars();
        }
    }

    /**
     * Hide the status and navigation bars. Sticky/transient on purpose: an edge
     * swipe brings them back briefly so the player can still leave the app, then
     * they hide again by themselves.
     */
    private void hideSystemBars() {
        final Window w = getWindow();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            w.setDecorFitsSystemWindows(false);
            WindowInsetsController c = w.getInsetsController();
            if (c != null) {
                c.hide(WindowInsets.Type.statusBars() | WindowInsets.Type.navigationBars());
                c.setSystemBarsBehavior(
                        WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
            }
        } else {
            w.getDecorView().setSystemUiVisibility(
                    View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                            | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY);
        }
    }
}
