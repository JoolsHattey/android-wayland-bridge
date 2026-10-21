import com.android.ide.common.vectordrawable.Svg2Vector
import java.io.ByteArrayOutputStream

plugins {
    id("com.android.application")
}

// The app of one Linux app: its launcher icon, the Android tasks of its
// windows, and its URL schemes. Built as
//   gradlew :stub:assembleDebug -Pstub.appId=<desktop id> -Pstub.label=<name> -Pstub.icon=<png or svg> \
//       -Pstub.schemes=<scheme,...> -Pstub.stamp=<stamp>
// scripts/install-stub.sh fills these in from the chroot's desktop file.
// The template build type (assembleTemplate, which dev.awb ships as an
// asset) is the one stubs are made from on the device instead: see
// dev.awb.StubMaker and src/template.
//
// It has no window code of its own: its activities are dev.awb's (:window),
// loaded at run time from the installed dev.awb (StubComponentFactory), so
// stubs follow dev.awb's updates. Only a change to what this manifest
// declares needs new stubs: bump stubFormat (and Window.STUB_FORMAT), and
// the device remakes the installed ones (awb-stubs.sh sync).
val stubFormat = 2
val linuxAppId = providers.gradleProperty("stub.appId").getOrElse("org.gnome.Adwaita1.Demo")
val label = providers.gradleProperty("stub.label").getOrElse(linuxAppId)
val icon = providers.gradleProperty("stub.icon").orNull
/** What the stub was built from (desktop file and icon): the device remakes it when this changes. */
val stamp = providers.gradleProperty("stub.stamp").getOrElse("")
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
        versionCode = stubFormat
        versionName = if (stamp.isEmpty()) "$stubFormat" else "$stubFormat-$stamp"
        manifestPlaceholders["linuxAppId"] = linuxAppId
        manifestPlaceholders["stubFormat"] = stubFormat.toString()
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
        create("template") {
            initWith(getByName("debug"))
        }
    }

    // The schemes' intent filter, merged into the launcher activity.
    for (type in listOf("debug", "release")) {
        sourceSets[type].manifest.srcFile(layout.buildDirectory.file("generated/stubManifest/AndroidManifest.xml").get().asFile)
    }

    for (type in listOf("debug", "release")) {
        sourceSets[type].res.directories.add(
            if (icon != null) layout.buildDirectory.dir("generated/stubIcon").get().asFile.path else "src/default/res",
        )
    }
    // The default icon, plus the PNG icon slot and the placeholder scheme.
    sourceSets["template"].res.directories.add("src/default/res")
}

// The Linux app's icon, as the foreground of the adaptive icon: a PNG as is,
// an SVG converted to a vector drawable (with Android Studio's converter,
// so no SVG renderer is needed on the host).
val stubIcon by tasks.registering {
    val src = icon
    val out = layout.buildDirectory.dir("generated/stubIcon/drawable-nodpi")
    inputs.property("icon", src ?: "")
    if (src != null) inputs.file(src)
    outputs.dir(out)
    doLast {
        val dir = out.get().asFile
        dir.deleteRecursively()
        dir.mkdirs()
        if (src == null) return@doLast
        if (src.endsWith(".svg")) {
            val xml = ByteArrayOutputStream()
            val errors = Svg2Vector.parseSvgToXml(File(src).toPath(), xml)
            if (errors.isNotEmpty()) System.err.println("$src: $errors")
            if (xml.size() == 0) throw GradleException("cannot convert $src")
            File(dir, "linux_icon.xml").writeBytes(xml.toByteArray())
        } else {
            File(src).copyTo(File(dir, "linux_icon.png"))
        }
    }
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

// Java only: no Kotlin stdlib in every stub.
configurations.configureEach { exclude(group = "org.jetbrains.kotlin") }
