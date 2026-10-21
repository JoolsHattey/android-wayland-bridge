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

dependencies {
    implementation(project(":window"))
}

tasks.named("preBuild") { dependsOn(cargoBuild) }
