plugins {
    kotlin("jvm") version "2.0.21"
    `java-library`
}

repositories {
    mavenCentral()
}

dependencies {
    api("net.java.dev.jna:jna:5.14.0")
    api("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0")

    testImplementation(kotlin("test"))
    testImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.9.0")
}

kotlin {
    jvmToolchain(21)
}

tasks.test {
    useJUnitPlatform()
    // The Drive9MobileClient currently opens its own multi-thread Tokio runtime,
    // which spawns OS threads. Keep tests sequential to keep the output readable.
    maxParallelForks = 1
}
