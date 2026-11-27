import org.jetbrains.kotlin.gradle.dsl.JvmTarget
import org.jetbrains.kotlin.gradle.dsl.KotlinVersion

plugins {
    id("com.android.library")
    id("org.jetbrains.dokka")
    `maven-publish`
}

group = providers.gradleProperty("GROUP").get()
version = providers.gradleProperty("VERSION_NAME").get()

// Set in GitHub Actions; absent on a developer machine.
val githubRepository: String? = System.getenv("GITHUB_REPOSITORY")

android {
    namespace = "dev.usbdac.media3"
    compileSdk = 36
    buildToolsVersion = "36.0.0"

    defaultConfig {
        minSdk = 26
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
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
        // Keep the published metadata readable by consumers on an older Kotlin.
        languageVersion.set(KotlinVersion.KOTLIN_2_2)
        apiVersion.set(KotlinVersion.KOTLIN_2_2)
    }
}

dependencies {
    api(project(":usbdac"))
    api("androidx.media3:media3-exoplayer:1.11.1")
    // StateFlow, SharedFlow and Flow are part of the public API.
    api("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.11.0")

    testImplementation("junit:junit:4.13.2")
    testImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.11.0")
    // The JVM has no org.json; Android supplies it at runtime.
    testImplementation("org.json:json:20260814")
}

// ---- publishing ---------------------------------------------------------------

publishing {
    publications {
        register<MavenPublication>("release") {
            groupId = project.group.toString()
            artifactId = "usbdac-media3"
            version = project.version.toString()
            afterEvaluate { from(components["release"]) }

            pom {
                name.set("usbdac-media3")
                description.set(
                    "Optional precision-preserving Media3 audio sink and DAC session volume for usbdac.",
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
