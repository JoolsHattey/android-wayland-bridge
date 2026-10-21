plugins {
    id("com.android.library")
}

// The window side of a toplevel: the activity, its surface and input. Shared
// by dev.awb and the stub apps, and talks to the compositor over binder.
// Compiled into dev.awb only: stubs load it from dev.awb's APK at run time,
// with their own resources, so this code must not use its own (R).
android {
    namespace = "dev.awb.window"
    compileSdk = 37

    defaultConfig {
        minSdk = 35
    }

    buildFeatures {
        aidl = true
    }
}
