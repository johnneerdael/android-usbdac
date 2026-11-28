import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}
val validatorVersionName = providers.gradleProperty("VERSION_NAME").get()
// 0.1.21 -> 1021: every release installs over the previous one.
val validatorVersionCode = validatorVersionName.substringBefore('-').split('.').map(String::toInt)
    .let { (major, minor, patch) -> major * 1_000_000 + minor * 1_000 + patch }
// Release builds sign with one pinned key, supplied by CI from repository
// secrets, so an installed validator upgrades in place. Without it, the
// machine's own debug key is used.
val pinnedKeystore = providers.environmentVariable("VALIDATOR_KEYSTORE").orNull
android {
    namespace = "dev.usbdac.validator"
    compileSdk = 36
    buildFeatures { aidl = true; compose = true; buildConfig = true }
    defaultConfig { applicationId = "dev.usbdac.validator"; minSdk = 26; targetSdk = 34; versionCode = validatorVersionCode; versionName = validatorVersionName }
    if (pinnedKeystore != null) {
        val password = providers.environmentVariable("VALIDATOR_KEYSTORE_PASSWORD").get()
        val pinned = signingConfigs.create("pinned") {
            storeFile = file(pinnedKeystore)
            storePassword = password
            keyAlias = "usbdac-validator"
            keyPassword = password
        }
        buildTypes.getByName("debug").signingConfig = pinned
    }
    compileOptions { sourceCompatibility = JavaVersion.VERSION_17; targetCompatibility = JavaVersion.VERSION_17 }
}
kotlin { compilerOptions { jvmTarget.set(JvmTarget.JVM_17) } }
dependencies {
    implementation(project(":usbdac"))
    implementation("dev.rikka.shizuku:api:13.1.5")
    implementation("dev.rikka.shizuku:provider:13.1.5")
    implementation(platform("androidx.compose:compose-bom:2026.06.01"))
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.activity:activity-compose:1.13.0")
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20250517")
}
