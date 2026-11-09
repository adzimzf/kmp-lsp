// kmp-debug-server.jar: Microsoft's java-debug core (the Debug Adapter
// Protocol implementation jdtls loads as a plugin) with the providers jdtls
// would back by the JDT Java model backed by kmp-lsp instead.
//
// Plain JVM jar, not a native image: the debugger needs the JDK's JDI
// (`jdk.jdi` module), so it runs on the user's JDK via `java -jar`.
plugins {
    kotlin("jvm")
    id("com.github.johnrengelman.shadow")
    application
}

group = "io.github.hessesian"
version = "1.0.0"

repositories {
    mavenCentral()
}

dependencies {
    implementation("com.microsoft.java:com.microsoft.java.debug.core:0.53.1")
    testImplementation("org.junit.jupiter:junit-jupiter:5.11.3")
    testRuntimeOnly("org.junit.platform:junit-platform-launcher")
}

application {
    mainClass.set("io.github.hessesian.debugserver.MainKt")
}

kotlin {
    jvmToolchain(21)
}

tasks.test {
    useJUnitPlatform()
}

tasks.shadowJar {
    archiveBaseName.set("kmp-debug-server")
    archiveClassifier.set("")
    archiveVersion.set("")
    mergeServiceFiles()
}
