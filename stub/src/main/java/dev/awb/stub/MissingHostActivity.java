package dev.awb.stub;

import android.app.Activity;
import android.os.Bundle;
import android.widget.Toast;

/** Stands in for the window activities when dev.awb cannot provide them. */
public final class MissingHostActivity extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Toast.makeText(this, "Wayland Bridge (dev.awb) is missing or too old for this app", Toast.LENGTH_LONG).show();
        finishAndRemoveTask();
    }
}
