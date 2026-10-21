plugins {
    id("com.android.library")
}

// The window side of a toplevel: the activity, its surface and input. Shared
// by dev.awb and the stub apps, and talks to the compositor over binder.
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
