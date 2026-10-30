import java.security.KeyStore

plugins {
    id("com.android.application")
}

android {
    namespace = "dev.awb"
    compileSdk = 37
    ndkVersion = "30.0.16248370"

    defaultConfig {
        applicationId = "dev.awb"
        minSdk = 35
        targetSdk = 37
        versionCode = 1
        versionName = "0.1.0"
        ndk { abiFilters += "x86_64" }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    sourceSets["main"].jniLibs.directories.add(layout.buildDirectory.dir("rustJniLibs").get().asFile.path)
    sourceSets["main"].jniLibs.directories.add(layout.buildDirectory.dir("waypipe/jniLibs").get().asFile.path)
    // Extracted to the native library directory, so libwaypipe.so can run.
    packaging.jniLibs.useLegacyPackaging = true
    sourceSets["main"].assets.directories.add(layout.buildDirectory.dir("generated/stubTemplate").get().asFile.path)
    sourceSets["main"].assets.directories.add(layout.buildDirectory.dir("generated/vmGuest").get().asFile.path)
}

// Builds native/ (the Rust compositor) with cargo-ndk into build/rustJniLibs.
val cargoBuild by tasks.registering(Exec::class) {
    val nativeDir = rootProject.layout.projectDirectory.dir("native")
    val outDir = layout.buildDirectory.dir("rustJniLibs")
    workingDir(nativeDir)
    inputs.dir(nativeDir.dir("src"))
    inputs.file(nativeDir.file("Cargo.toml"))
    outputs.dir(outDir)
    commandLine("cargo", "ndk", "-t", "x86_64", "--platform", "35", "-o", outDir.get().asFile.path, "build", "--release")
}

// waypipe for Android, which carries the VM's windows (scripts/build-waypipe.sh).
val waypipeBuild by tasks.registering(Exec::class) {
    val script = rootProject.layout.projectDirectory.file("scripts/build-waypipe.sh")
    val outDir = layout.buildDirectory.dir("waypipe/jniLibs")
    inputs.file(script)
    outputs.dir(outDir)
    val ndk = androidComponents.sdkComponents.ndkDirectory
    executable("bash")
    argumentProviders.add(CommandLineArgumentProvider {
        listOf(script.asFile.path, ndk.get().asFile.path, outDir.get().asFile.path)
    })
}

// The VM guest's side (vm/), as assets/vm/ next to src/main/assets/vm
// (provision.sh, the unit): VmCiData puts it on the cidata disk, from which
// the guest installs it on first boot. awb-display is built for the guest
// (Debian, glibc), not for Android.
val awbDisplayBuild by tasks.registering(Exec::class) {
    val crate = rootProject.layout.projectDirectory.dir("vm/awb-display")
    workingDir(crate)
    inputs.dir(crate.dir("src"))
    inputs.files(crate.file("Cargo.toml"), crate.file("Cargo.lock"))
    outputs.file(crate.file("target/x86_64-unknown-linux-gnu/release/awb-display"))
    commandLine("cargo", "build", "--release", "--target", "x86_64-unknown-linux-gnu")
}

val vmGuest by tasks.registering(Sync::class) {
    val vm = rootProject.layout.projectDirectory.dir("vm")
    from(awbDisplayBuild)
    from(vm.file("awb-run"))
    into(layout.buildDirectory.dir("generated/vmGuest/vm"))
}

// The template that StubMaker makes stubs from on the device: the stub
// module's template build, as assets/stub-template.apk.
val stubTemplate by tasks.registering(Copy::class) {
    dependsOn(":stub:assembleTemplate")
    from(project(":stub").layout.buildDirectory.file("outputs/apk/template/stub-template.apk"))
    into(layout.buildDirectory.dir("generated/stubTemplate"))
}

// The debug signing key, for StubMaker to sign stubs with on the device:
// deploy.sh pushes it to /data/adb/awb (root only). PKCS#8 key and DER
// certificate, which Android reads without a keystore provider.
val stubKey by tasks.registering {
    val config = android.signingConfigs.getByName("debug")
    val store = config.storeFile!!
    val storePassword = config.storePassword!!
    val alias = config.keyAlias!!
    val keyPassword = config.keyPassword!!
    val out = layout.buildDirectory.dir("stubKey")
    inputs.file(store)
    outputs.dir(out)
    doLast {
        val ks = KeyStore.getInstance(KeyStore.getDefaultType())
        store.inputStream().use { ks.load(it, storePassword.toCharArray()) }
        val dir = out.get().asFile
        dir.mkdirs()
        File(dir, "stub-key.pk8").writeBytes(ks.getKey(alias, keyPassword.toCharArray()).encoded)
        File(dir, "stub-cert.der").writeBytes(ks.getCertificate(alias).encoded)
    }
}

dependencies {
    implementation(project(":window"))
    // Signs the stubs StubMaker makes (pure Java, as in apksigner).
    implementation("com.android.tools.build:apksig:9.3.2")
}

tasks.named("preBuild") { dependsOn(cargoBuild, waypipeBuild, stubTemplate, vmGuest) }
