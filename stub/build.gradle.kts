plugins {
    id("com.android.application")
}

// The app of one Linux app: its launcher icon, and the Android tasks of its
// windows (the activities come from :window). Hand-built for now:
//   gradlew :stub:assembleDebug -Pstub.appId=<desktop id> -Pstub.label=<name> -Pstub.icon=<png>
// scripts/install-stub.sh fills these in from the chroot's desktop file.
val linuxAppId = providers.gradleProperty("stub.appId").getOrElse("org.gnome.Adwaita1.Demo")
val label = providers.gradleProperty("stub.label").getOrElse(linuxAppId)
val icon = providers.gradleProperty("stub.icon").orNull

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

    sourceSets["main"].res.directories.add(
        if (icon != null) layout.buildDirectory.dir("generated/stubIcon").get().asFile.path else "src/default/res",
    )
}

// The Linux app's icon, as the foreground of the adaptive icon.
val stubIcon by tasks.registering(Copy::class) {
    if (icon != null) from(icon) { rename { "linux_icon.png" } }
    into(layout.buildDirectory.dir("generated/stubIcon/drawable-nodpi"))
}

tasks.named("preBuild") { dependsOn(stubIcon) }

dependencies {
    implementation(project(":window"))
}
