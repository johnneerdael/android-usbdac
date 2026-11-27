plugins { id("com.android.application") }
android {
 namespace = "dev.usbdac.media3.example"
 compileSdk = 36
 defaultConfig { applicationId = "dev.usbdac.media3.example"; minSdk = 26; targetSdk = 36; versionCode = 1; versionName = providers.gradleProperty("VERSION_NAME").get() }
 compileOptions { sourceCompatibility = JavaVersion.VERSION_17; targetCompatibility = JavaVersion.VERSION_17 }
}
dependencies {
 implementation(project(":usbdac-media3"))
 implementation("androidx.media3:media3-session:1.11.1")
 implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.11.0")
}
