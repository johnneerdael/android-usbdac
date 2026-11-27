import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins { id("com.android.application") }
android {
    namespace = "dev.usbdac.validator"
    compileSdk = 36
    buildFeatures { aidl = true }
    defaultConfig { applicationId = "dev.usbdac.validator"; minSdk = 26; targetSdk = 34; versionCode = 1; versionName = providers.gradleProperty("VERSION_NAME").get() }
    compileOptions { sourceCompatibility = JavaVersion.VERSION_17; targetCompatibility = JavaVersion.VERSION_17 }
}
kotlin { compilerOptions { jvmTarget.set(JvmTarget.JVM_17) } }
dependencies {
    implementation(project(":usbdac"))
    implementation("dev.rikka.shizuku:api:13.1.5")
    implementation("dev.rikka.shizuku:provider:13.1.5")
}
