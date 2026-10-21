package dev.awb.stub;

import android.app.Activity;
import android.app.AppComponentFactory;
import android.app.Application;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.util.Log;

/**
 * Makes the stub's activities out of dev.awb's installed code, so that a
 * stub never goes stale: updating dev.awb updates the window side of every
 * stub. The stub itself is only a manifest (names, icon, intent filters,
 * activity attributes) and this class. Java, not Kotlin: no stdlib to carry.
 *
 * The code comes from {@link Context#createPackageContext} with
 * {@link Context#CONTEXT_INCLUDE_CODE}, which also registers dev.awb as a
 * package this process depends on: Android kills it when dev.awb is updated,
 * so no stub keeps running old window code against a new compositor.
 */
public final class StubComponentFactory extends AppComponentFactory {
    static final String HOST = "dev.awb";
    static final String WINDOW_PACKAGE = "dev.awb.window.";
    static final String TAG = "awb";

    private Application app;
    private ClassLoader host;

    @Override
    public Application instantiateApplication(ClassLoader cl, String className)
            throws InstantiationException, IllegalAccessException, ClassNotFoundException {
        app = super.instantiateApplication(cl, className);
        return app;
    }

    @Override
    public Activity instantiateActivity(ClassLoader cl, String className, Intent intent)
            throws InstantiationException, IllegalAccessException, ClassNotFoundException {
        if (!className.startsWith(WINDOW_PACKAGE)) return super.instantiateActivity(cl, className, intent);
        ClassLoader loader = host();
        if (loader == null) return new MissingHostActivity();
        try {
            return (Activity) loader.loadClass(className).getDeclaredConstructor().newInstance();
        } catch (ReflectiveOperationException e) {
            // A dev.awb that no longer has this activity: the stub is too old.
            Log.e(TAG, HOST + " has no " + className, e);
            return new MissingHostActivity();
        }
    }

    /** dev.awb's class loader, or null if it is not installed. Main thread only. */
    private ClassLoader host() {
        if (host == null) {
            try {
                host = app.createPackageContext(HOST, Context.CONTEXT_INCLUDE_CODE | Context.CONTEXT_IGNORE_SECURITY)
                    .getClassLoader();
            } catch (PackageManager.NameNotFoundException e) {
                Log.e(TAG, "cannot load the window code: " + HOST + " is not installed");
            }
        }
        return host;
    }
}
