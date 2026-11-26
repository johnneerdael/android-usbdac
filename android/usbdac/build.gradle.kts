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

android {
    namespace = "dev.usbdac"
    compileSdk = 36
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

// ---- native library ---------------------------------------------------------

// ABIs to build. Restrict for a quick local build: -Pusbdac.abis=arm64-v8a
val abis = providers.gradleProperty("usbdac.abis")
    .map { list -> list.split(',').map { it.trim() }.filter { it.isNotEmpty() } }
    .orElse(listOf("arm64-v8a", "armeabi-v7a", "x86_64", "x86"))

val repoRoot = rootProject.projectDir.parentFile
val jniLibsDir = layout.projectDirectory.dir("src/main/jniLibs")
val ndkDirectory = androidComponents.sdkComponents.ndkDirectory

val buildUsbdacNative by tasks.registering(Exec::class) {
    group = "build"
    description = "Builds libusbdac.so for the selected ABIs with cargo-ndk into src/main/jniLibs."

    workingDir = repoRoot

    inputs.property("abis", abis)
    inputs.property("ndkVersion", ndkVersionPin)
    inputs.files(
        fileTree(repoRoot) {
            include("Cargo.toml", "Cargo.lock", "crates/**/Cargo.toml", "crates/**/*.rs")
        },
    ).withPropertyName("rustSources").withPathSensitivity(PathSensitivity.RELATIVE)
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
        commandLine(arguments)
    }
}

tasks.named("preBuild") { dependsOn(buildUsbdacNative) }

// ---- documentation ------------------------------------------------------------

dokka {
    moduleName.set("usbdac")
    dokkaSourceSets.configureEach {
        includes.from("Module.md")
    }
}

// ---- publishing ---------------------------------------------------------------

// Set in GitHub Actions; absent on a developer machine.
val githubRepository: String? = System.getenv("GITHUB_REPOSITORY")

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
        maven {
            name = "GitHubPackages"
            url = uri("https://maven.pkg.github.com/${githubRepository ?: "OWNER/usbdac"}")
            credentials {
                username = System.getenv("GITHUB_ACTOR")
                password = System.getenv("GITHUB_TOKEN")
            }
        }
    }
}
