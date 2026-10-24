// Termux 的 terminal-view（vendored，见 android/README.md）
plugins {
    id("com.android.library")
}

android {
    namespace = "com.termux.view"
    compileSdk = 37

    defaultConfig {
        minSdk = 29
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    implementation("androidx.annotation:annotation:1.11.0")
    api(project(":terminal-emulator"))
}
