plugins {
    id("com.android.application")
}

// The app of one Linux app: its launcher icon, the Android tasks of its
// windows (the activities come from :window), and its URL schemes. Built as
//   gradlew :stub:assembleDebug -Pstub.appId=<desktop id> -Pstub.label=<name> -Pstub.icon=<png> \
//       -Pstub.schemes=<scheme,...>
// scripts/install-stub.sh fills these in from the chroot's desktop file.
val linuxAppId = providers.gradleProperty("stub.appId").getOrElse("org.gnome.Adwaita1.Demo")
val label = providers.gradleProperty("stub.label").getOrElse(linuxAppId)
val icon = providers.gradleProperty("stub.icon").orNull
/** The app's x-scheme-handler types: links in these open in the Linux app. */
val schemes = providers.gradleProperty("stub.schemes").getOrElse("")
    .split(',').map { it.trim().lowercase() }.filter { it.matches(Regex("[a-z][a-z0-9+.-]*")) }

/** Package name of a desktop file id: dev.awb.app.<id>, made a valid Java name. */
val stubPackage = "dev.awb.app." + linuxAppId.split('.').joinToString(".") { part ->
    val clean = part.replace(Regex("[^A-Za-z0-9_]"), "_")
    if (clean.firstOrNull()?.isLetter() == true) clean else "_$clean"
}

android {
    namespace = "dev.awb.stub"
    compileSdk = 37

    defaultConfig {
        applicationId = stubPackage
        minSdk = 35
        targetSdk = 37
        versionCode = 1
        versionName = "0.1.0"
        manifestPlaceholders["linuxAppId"] = linuxAppId
        resValue("string", "app_name", label)
    }

    buildFeatures {
        resValues = true
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    // The schemes' intent filter, merged into the launcher activity.
    for (type in listOf("debug", "release")) {
        sourceSets[type].manifest.srcFile(layout.buildDirectory.file("generated/stubManifest/AndroidManifest.xml").get().asFile)
    }

    sourceSets["main"].res.directories.add(
        if (icon != null) layout.buildDirectory.dir("generated/stubIcon").get().asFile.path else "src/default/res",
    )
}

// The Linux app's icon, as the foreground of the adaptive icon.
val stubIcon by tasks.registering(Copy::class) {
    if (icon != null) from(icon) { rename { "linux_icon.png" } }
    into(layout.buildDirectory.dir("generated/stubIcon/drawable-nodpi"))
}

val stubManifest by tasks.registering {
    val out = layout.buildDirectory.file("generated/stubManifest/AndroidManifest.xml")
    // Locals only: the action must not capture the build script.
    val linkSchemes = schemes
    inputs.property("schemes", linkSchemes)
    outputs.file(out)
    doLast {
        val data = linkSchemes.joinToString("\n") { "                <data android:scheme=\"$it\" />" }
        val filter = if (linkSchemes.isEmpty()) "" else """
            <intent-filter>
                <action android:name="android.intent.action.VIEW" />
                <category android:name="android.intent.category.DEFAULT" />
                <category android:name="android.intent.category.BROWSABLE" />
$data
            </intent-filter>"""
        out.get().asFile.writeText(
            """
            |<?xml version="1.0" encoding="utf-8"?>
            |<manifest xmlns:android="http://schemas.android.com/apk/res/android">
            |    <application>
            |        <activity android:name="dev.awb.window.LaunchActivity">$filter
            |        </activity>
            |    </application>
            |</manifest>
            |""".trimMargin(),
        )
    }
}

tasks.named("preBuild") { dependsOn(stubIcon, stubManifest) }

dependencies {
    implementation(project(":window"))
}
