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
    sourceSets["main"].assets.directories.add(layout.buildDirectory.dir("generated/stubTemplate").get().asFile.path)
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

tasks.named("preBuild") { dependsOn(cargoBuild, stubTemplate) }
