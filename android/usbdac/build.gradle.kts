import org.jetbrains.kotlin.gradle.dsl.JvmTarget
import org.jetbrains.kotlin.gradle.dsl.KotlinVersion

plugins {
    id("com.android.library")
    id("org.jetbrains.dokka")
    `maven-publish`
}

group = providers.gradleProperty("GROUP").get()
version = providers.gradleProperty("VERSION_NAME").get()

val ndkVersionPin = "28.2.13676358"

// Set in GitHub Actions; absent on a developer machine.
val githubRepository: String? = System.getenv("GITHUB_REPOSITORY")

android {
    namespace = "dev.usbdac"
    compileSdk = 36
    buildToolsVersion = "36.0.0"
    ndkVersion = ndkVersionPin

    defaultConfig {
        minSdk = 26
        // R8 must keep the JNI surface of libusbdac.so: the native code looks
        // these classes and methods up by name.
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    testOptions {
        // Pure-logic tests may reach android.util.Log; return defaults instead of throwing.
        unitTests.isReturnDefaultValues = true
    }

    publishing {
        singleVariant("release") {
            withSourcesJar()
        }
    }
}

kotlin {
    // Every public declaration states its visibility and type, and is documented.
    explicitApi()
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
        // Keep the published metadata readable by consumers on an older Kotlin.
        languageVersion.set(KotlinVersion.KOTLIN_2_2)
        apiVersion.set(KotlinVersion.KOTLIN_2_2)
    }
}

dependencies {
    // StateFlow, SharedFlow and Flow are part of the public API.
    api("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.11.0")

    testImplementation("junit:junit:4.13.2")
    testImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.11.0")
    // The JVM has no org.json; Android supplies it at runtime.
    testImplementation("org.json:json:20260814")
}

// The named host-JNI test must rerun when its native ranking implementation changes.
tasks.withType<Test>().configureEach {
    val hostJni = providers.environmentVariable("USBDAC_TEST_HOST_JNI")
    inputs.property("usbdacHostJniPath", hostJni.orElse(""))
    inputs.files(hostJni.map { listOf(file(it)) }.orElse(emptyList())).withPropertyName("usbdacHostJniLibrary")
}

// ---- native library ---------------------------------------------------------

// ABIs to build. Restrict for a quick local build: -Pusbdac.abis=arm64-v8a
val abis = providers.gradleProperty("usbdac.abis")
    .map { list -> list.split(',').map { it.trim() }.filter { it.isNotEmpty() } }
    .orElse(listOf("arm64-v8a", "armeabi-v7a", "x86_64", "x86"))

// -Pusbdac.skipNative=true leaves the native build out, for jobs that only need the Kotlin side
// (the documentation build). An AAR built that way has no libusbdac.so: never publish one.
val skipNative = providers.gradleProperty("usbdac.skipNative").map { it.toBoolean() }.orElse(false)

val repoRoot = rootProject.projectDir.parentFile
val jniLibsDir = layout.projectDirectory.dir("src/main/jniLibs")
val ndkDirectory = androidComponents.sdkComponents.ndkDirectory
val validatorEnabled = gradle.startParameter.taskNames.any { it.contains("validator", ignoreCase = true) } ||
    providers.gradleProperty("usbdac.validator").orNull == "true"

val buildUsbdacNative by tasks.registering(Exec::class) {
    group = "build"
    description = "Builds libusbdac.so for the selected ABIs with cargo-ndk into src/main/jniLibs."

    workingDir = repoRoot
    onlyIf { !skipNative.get() }

    inputs.property("validatorEnabled", validatorEnabled)
    inputs.property("abis", abis)
    inputs.property("ndkVersion", ndkVersionPin)
    inputs.files(
        fileTree(repoRoot) {
            include("Cargo.toml", "Cargo.lock", "crates/**/Cargo.toml", "crates/**/*.rs")
        },
    ).withPropertyName("rustSources").withPathSensitivity(PathSensitivity.RELATIVE)
    if (validatorEnabled) {
        inputs.files(
            fileTree(repoRoot) {
                include(
                    "crates/usbdac-validator/src/report_html/template.html",
                    "branding/tokens/usbdac.css",
                    "branding/logos/usbdac-mark-dark.svg",
                    "branding/logos/usbdac-mark-light.svg",
                    "branding/fonts/Manrope.woff",
                    "branding/fonts/IBMPlexMono-Regular.woff",
                    "branding/fonts/manrope-OFL.txt",
                    "branding/fonts/ibmplexmono-OFL.txt",
                )
            },
        ).withPropertyName("validatorReportAssets").withPathSensitivity(PathSensitivity.RELATIVE)
    }
    outputs.dir(jniLibsDir).withPropertyName("jniLibs")

    doFirst {
        // Drop ABIs left over from an earlier, wider build so the AAR holds exactly what was asked for.
        delete(jniLibsDir)
        val cargoBin = "${System.getProperty("user.home")}/.cargo/bin"
        environment("PATH", cargoBin + File.pathSeparator + System.getenv("PATH").orEmpty())
        environment("ANDROID_NDK_HOME", ndkDirectory.get().asFile.absolutePath)
        val arguments = mutableListOf("cargo", "ndk")
        abis.get().forEach { arguments += listOf("-t", it) }
        arguments += listOf("-o", jniLibsDir.asFile.absolutePath, "build", "-p", "usbdac-jni", "--release")
        if (validatorEnabled) arguments += listOf("--features", "validator")
        commandLine(arguments)
    }
}

tasks.named("preBuild") { dependsOn(buildUsbdacNative) }

// ---- documentation ------------------------------------------------------------

dokka {
    moduleName.set("usbdac")
    // The usbdac brand on the API reference: logo, fonts and colours (docs/site/dokka).
    pluginsConfiguration.html {
        val site = rootProject.projectDir.resolve("../docs/site/dokka")
        customStyleSheets.from(site.resolve("usbdac-dokka.css"))
        customAssets.from(site.resolve("logo-icon.svg"), rootProject.projectDir.resolve("../branding/fonts/Manrope.woff"))
        footerMessage.set("usbdac · Direct USB audio. Clear evidence. · MIT licensed")
    }
    dokkaSourceSets.configureEach {
        includes.from("Module.md")
        if (githubRepository != null) {
            sourceLink {
                localDirectory.set(layout.projectDirectory.dir("src/main/kotlin"))
                remoteUrl("https://github.com/$githubRepository/blob/main/android/usbdac/src/main/kotlin")
                remoteLineSuffix.set("#L")
            }
        }
    }
}

// ---- publishing ---------------------------------------------------------------

publishing {
    publications {
        register<MavenPublication>("release") {
            groupId = project.group.toString()
            artifactId = providers.gradleProperty("POM_ARTIFACT_ID").get()
            version = project.version.toString()
            afterEvaluate { from(components["release"]) }

            pom {
                name.set("usbdac")
                description.set(
                    "Bit-perfect direct USB DAC output for Android: a Kotlin SDK over a Rust USB Audio Class driver.",
                )
                if (githubRepository != null) {
                    url.set("https://github.com/$githubRepository")
                    scm {
                        url.set("https://github.com/$githubRepository")
                        connection.set("scm:git:https://github.com/$githubRepository.git")
                    }
                }
                licenses {
                    license {
                        name.set("MIT License")
                        url.set("https://opensource.org/licenses/MIT")
                        distribution.set("repo")
                    }
                }
            }
        }
    }
    repositories {
        // A local copy of exactly what GitHub Packages receives; the release workflow attaches these files.
        maven {
            name = "Staging"
            url = uri(rootProject.layout.buildDirectory.dir("staging"))
        }
        maven {
            name = "GitHubPackages"
            url = uri("https://maven.pkg.github.com/${githubRepository ?: "johnneerdael/android-usbdac"}")
            credentials {
                username = System.getenv("GITHUB_ACTOR")
                password = System.getenv("GITHUB_TOKEN")
            }
        }
    }
}
