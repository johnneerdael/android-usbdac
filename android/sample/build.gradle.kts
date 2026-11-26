import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
}

android {
    namespace = "dev.usbdac.sample"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.usbdac.sample"
        minSdk = 26
        targetSdk = 34 // below 35 so the plain-View layout is not forced edge-to-edge
        versionCode = 1
        versionName = providers.gradleProperty("VERSION_NAME").get()
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildTypes {
        release {
            // The sample is built unsigned; it exists to show the SDK, not to be shipped.
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"))
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
    }
}

dependencies {
    implementation(project(":usbdac"))
    // Dispatchers.Main for the activity.
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.11.0")
}
