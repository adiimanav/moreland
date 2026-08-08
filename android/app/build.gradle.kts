plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.moreland.display"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.moreland.display"
        // API 29 is the floor for LocalServerSocket usage as written; the
        // target tablet is API 34.
        minSdk = 29
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // Signed with the debug key so `adb install` works without
            // provisioning a keystore. This app is sideloaded, never published.
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }
}
