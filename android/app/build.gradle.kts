import groovy.json.JsonSlurper
import org.gradle.api.artifacts.component.ModuleComponentIdentifier
import org.gradle.api.artifacts.result.ResolvedComponentResult
import org.gradle.api.artifacts.result.ResolvedDependencyResult
import java.io.ByteArrayOutputStream
import java.io.File
import java.util.zip.GZIPInputStream
import javax.inject.Inject

plugins {
    id("com.android.application")
}

/** Repository root: the Cargo workspace and the rootfs from `scripts/fetch-rootfs` are here. */
val repoRoot: Directory = rootProject.layout.projectDirectory.dir("..")

/** Alpine minirootfs: the tarball `scripts/fetch-rootfs` downloaded and verified. */
val rootfsTarball: File? = repoRoot.dir("target/rootfs").asFile
    .listFiles { f -> f.name.matches(Regex("alpine-minirootfs-.*-aarch64\\.tar\\.gz")) }
    ?.maxByOrNull { it.name }

/**
 * The doas package bundled with the app (downloaded by `scripts/fetch-rootfs`), installed offline
 * when a regular user first becomes the login account.
 */
val doasPackage: File? = repoRoot.dir("target/rootfs").asFile
    .listFiles { f -> f.name.matches(Regex("doas-.*\\.apk")) }
    ?.maxByOrNull { it.name }

/** For example 3.24.3 → v3.24, written into apk's repository URLs. */
val alpineBranch: String = rootfsTarball
    ?.name?.let { Regex("""alpine-minirootfs-(\d+)\.(\d+)\.""").find(it) }
    ?.let { "v${it.groupValues[1]}.${it.groupValues[2]}" }
    ?: "edge"

android {
    namespace = "io.github.mthli.appine"
    compileSdk = 37
    ndkVersion = "29.0.14206865"

    defaultConfig {
        applicationId = "io.github.mthli.appine"
        minSdk = 29
        targetSdk = 37
        versionCode = 1
        versionName = "0.0.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        buildConfigField("String", "ALPINE_BRANCH", "\"$alpineBranch\"")
    }

    buildFeatures {
        buildConfig = true
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // Installed locally only: signed with the debug key
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        // Native libraries are stored uncompressed and page-aligned in the APK and mapped directly
        // from it (also on 16K-page devices)
        jniLibs.useLegacyPackaging = false
    }
}

/**
 * Builds libappine.so (hosts/android) with cargo-ndk. The Rust part is always a release build: the
 * interpreter of a debug build is too slow. The debug app turns on the `test-hooks` feature, so
 * instrumentation tests use the test hooks to simulate kernel-layer and engine bugs in the app
 * process (effective only when `APPINE_TEST_HOOKS=1` is set); it also turns on `borrow-check`,
 * which checks that no kernel object is borrowed when the BKL is released.
 */
abstract class CargoNdkTask : DefaultTask() {
    @get:Internal
    abstract val workspace: DirectoryProperty

    @get:Input
    abstract val ndkDirectory: Property<String>

    @get:Input
    abstract val features: ListProperty<String>

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @get:Inject
    abstract val exec: ExecOperations

    init {
        // cargo does its own incremental builds
        outputs.upToDateWhen { false }
    }

    @TaskAction
    fun build() {
        val root = workspace.get().asFile
        val cargo = File(System.getProperty("user.home"), ".cargo/bin/cargo")
            .takeIf { it.canExecute() }?.path ?: "cargo"
        exec.exec {
            workingDir = root
            executable = cargo
            args(
                "ndk", "-t", "arm64-v8a", "--platform", "29",
                "-o", outputDir.get().asFile.path,
                "build", "--release", "-p", "appine-android",
            )
            if (features.get().isNotEmpty()) args("--features", features.get().joinToString(","))
            // Set both: with only one set, when the other one in the environment points at another
            // NDK (GitHub's runners preset ANDROID_NDK_ROOT), cargo-ndk warns, and which one it
            // uses depends on its order of precedence
            environment("ANDROID_NDK_HOME", ndkDirectory.get())
            environment("ANDROID_NDK_ROOT", ndkDirectory.get())
            environment("CARGO_TARGET_DIR", File(root, "target/android").path)
        }
    }
}

/**
 * Puts the rootfs tarball into the assets (imported on first start, the android/setup map), and the
 * doas package too (for the login account).
 */
abstract class RootfsAssetTask : DefaultTask() {
    @get:InputFile
    @get:Optional
    abstract val tarball: RegularFileProperty

    @get:InputFile
    @get:Optional
    abstract val doas: RegularFileProperty

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @TaskAction
    fun copy() {
        val source = tarball.orNull?.asFile
            ?: throw GradleException("Alpine minirootfs not found: run scripts/fetch-rootfs first")
        val out = outputDir.get().asFile
        out.deleteRecursively()
        out.mkdirs()
        source.copyTo(File(out, "rootfs.tgz"))
        val doasFile = doas.orNull?.asFile
            ?: throw GradleException("doas package not found: run scripts/fetch-rootfs first")
        doasFile.copyTo(File(out, "doas.apk"))
    }
}

/**
 * Generates the content of the "Open source licenses" page in Settings (design doc §13.4) under
 * `notices/` in the assets: each line of `index.tsv` is "title<TAB>description<TAB>file name", and
 * the files are plain text. In order: appine itself (AGPL), the terminal (libghostty-vt and the
 * libraries it links, and the JetBrains Mono font, from texts in `android/licenses/`), the packages
 * of the Alpine minirootfs in the APK, the Android libraries, the Rust crates linked into
 * libappine.so (with the license files each one publishes attached as they are), SQLite, and last
 * a few full license texts (`android/licenses/`).
 *
 * The build fails when a newly added Android library is not in one of the groups recognised below:
 * first confirm its license is compatible with AGPL-3.0, then add it.
 */
abstract class NoticesTask : DefaultTask() {
    @get:InputFile
    abstract val license: RegularFileProperty

    @get:InputDirectory
    abstract val texts: DirectoryProperty

    @get:InputFile
    @get:Optional
    abstract val tarball: RegularFileProperty

    @get:InputFile
    @get:Optional
    abstract val doas: RegularFileProperty

    @get:InputFile
    abstract val cargoLock: RegularFileProperty

    /** Android libraries in the APK, `group:name:version`. */
    @get:Input
    abstract val androidLibraries: ListProperty<String>

    @get:Internal
    abstract val workspace: DirectoryProperty

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @get:Inject
    abstract val exec: ExecOperations

    @TaskAction
    fun generate() {
        val root = outputDir.get().asFile
        root.deleteRecursively()
        val out = File(root, "notices").apply { mkdirs() }
        val index = StringBuilder()
        fun entry(title: String, subtitle: String, file: String, text: String) {
            File(out, file).writeText(text.trimEnd() + "\n")
            index.append(title).append('\t').append(subtitle).append('\t').append(file).append('\n')
        }
        val textDir = texts.get().asFile
        fun text(name: String) = File(textDir, name).readText()

        entry(
            "appine", "AGPL-3.0-or-later · $SOURCE", "appine.txt",
            wrap(APPINE_NOTICE) + "\n\n" + license.get().asFile.readText(),
        )
        // The terminal: libghostty-vt and what it links, and the font (texts in ../licenses, from
        // the sources scripts/fetch-ghostty downloads; change them together with its version)
        entry(
            "Ghostty 1.4.0 (libghostty-vt)", "MIT · the terminal emulator", "ghostty.txt",
            wrap(GHOSTTY_NOTICE) + "\n\n" + text("ghostty-LICENSE"),
        )
        entry(
            "uucode 0.2.0", "MIT; Unicode-3.0 (Unicode data)", "uucode.txt",
            text("uucode-LICENSE.md") + "\n\n----- licenses/LICENSE_unicode -----\n\n" + text("uucode-LICENSE_unicode") +
                "\n\n----- licenses/LICENSE_Bjoern_Hoehrmann -----\n\n" + text("uucode-LICENSE_Bjoern_Hoehrmann"),
        )
        entry(
            "simdutf 9.0.0", "MIT (also offered under Apache-2.0)", "simdutf.txt",
            "simdutf 9.0.0 (https://github.com/simdutf/simdutf), used under the MIT license:\n\n" + text("simdutf-LICENSE-MIT"),
        )
        entry(
            "Highway", "BSD-3-Clause (also offered under Apache-2.0)", "highway.txt",
            "Highway (https://github.com/google/highway) at commit 66486a10623fa0d72fe91260f96c892e41aceb06, " +
                "used under the BSD 3-Clause license:\n\n" + text("highway-LICENSE-BSD3"),
        )
        entry(
            "Wuffs", "MIT (also offered under Apache-2.0)", "wuffs.txt",
            "Wuffs (https://github.com/google/wuffs) at commit 7411f488fe2e2c205c3d3b3d28638b7356522930, " +
                "used under the MIT license:\n\n" + text("wuffs-LICENSE-MIT"),
        )
        entry(
            "Zig 0.17.0 compiler_rt", "MIT", "zig.txt",
            "compiler_rt of Zig 0.17.0 (https://ziglang.org), linked into libghostty-vt:\n\n" + text("zig-LICENSE"),
        )
        entry(
            "JetBrains Mono 2.304", "OFL-1.1 · the terminal font", "jetbrains-mono.txt",
            "JetBrains Mono 2.304 (https://github.com/JetBrains/JetBrainsMono), the variable upright and italic " +
                "fonts:\n\n" + text("JetBrainsMono-OFL.txt"),
        )
        alpine()?.let { (title, subtitle, body) -> entry(title, subtitle, "alpine.txt", body) }

        val libraries = androidLibraries.get()
        for (lib in libraries) {
            if (APACHE_GROUPS.none { lib.startsWith(it) }) {
                throw GradleException("notices: unknown license for Android library $lib; check it and add it to APACHE_GROUPS")
            }
        }
        entry(
            "Android libraries", "Apache-2.0 · AndroidX, Material Components, Kotlin, kotlinx.coroutines and others", "android.txt",
            wrap(
                "These libraries are built into the app and are licensed under the Apache License 2.0 " +
                    "(the full text is at the end of this list):",
            ) + "\n\n" + libraries.joinToString("\n"),
        )

        var sqlite: String? = null
        for (crate in crates()) {
            val name = crate["name"] as String
            val version = crate["version"] as String
            val expr = crate["license"] as String? ?: "see the license file"
            val dir = File(crate["manifest_path"] as String).parentFile
            val repository = crate["repository"] as String? ?: "https://crates.io/crates/$name"
            val body = StringBuilder("Rust crate $name $version ($repository)\nLicense: $expr\n")
            val files = licenseFiles(dir, expr)
            if (files.isEmpty()) {
                body.append("\nThe published crate includes no license file.\n")
                logger.warn("notices: $name $version has no license file")
            }
            for (f in files) body.append("\n----- ").append(f.name).append(" -----\n\n").append(f.readText().trimEnd()).append('\n')
            entry("$name $version", expr, "crate-$name-$version.txt", body.toString())
            if (name == "libsqlite3-sys") {
                sqlite = File(dir, "sqlite3/sqlite3.h").takeIf { it.isFile }?.useLines { lines ->
                    lines.firstNotNullOfOrNull { Regex("""#define SQLITE_VERSION\s+"([^"]+)"""").find(it)?.groupValues?.get(1) }
                }
            }
        }
        entry("SQLite ${sqlite ?: ""}".trim(), "Public domain (bundled by libsqlite3-sys)", "sqlite.txt", wrap(SQLITE_BLESSING))

        entry("GNU General Public License 3.0", "Full text", "GPL-3.0.txt", text("GPL-3.0.txt"))
        entry("GNU General Public License 2.0", "Full text", "GPL-2.0.txt", text("GPL-2.0.txt"))
        entry("Apache License 2.0", "Full text", "Apache-2.0.txt", text("Apache-2.0.txt"))
        entry("Mozilla Public License 2.0", "Full text", "MPL-2.0.txt", text("MPL-2.0.txt"))
        File(out, "index.tsv").writeText(index.toString())
    }

    /**
     * The Alpine minirootfs in the APK: title, description, body (each package's version, license,
     * aports commit).
     */
    private fun alpine(): Triple<String, String, String>? {
        val file = tarball.orNull?.asFile ?: return null
        val version = Regex("""alpine-minirootfs-([0-9.]+)-""").find(file.name)?.groupValues?.get(1) ?: "?"
        val installed = tarEntry(file, "lib/apk/db/installed")
            ?: throw GradleException("notices: no lib/apk/db/installed in ${file.name}")
        val rows = mutableListOf<List<String>>()
        var fields = mutableMapOf<Char, String>()
        for (line in (installed + "\n").lines()) {
            if (line.isEmpty()) {
                if ('P' in fields) {
                    rows.add(listOf('P', 'V', 'L', 'o', 'c').map { fields[it].orEmpty() })
                }
                fields = mutableMapOf()
            } else if (line.length > 2 && line[1] == ':') {
                fields[line[0]] = line.substring(2)
            }
        }
        val licenses = rows.map { it[2] }.distinct().sorted()
        val body = StringBuilder()
        body.append(
            wrap(
                "The app includes the Alpine Linux $version minirootfs and imports it on first launch. " +
                "It consists of the packages below. The source code of each package is its directory in " +
                "Alpine's aports repository at the commit given (the APKBUILD and patches, " +
                "https://gitlab.alpinelinux.org/alpine/aports/-/tree/<commit>/main/<origin>) together " +
                "with the upstream sources that it names, which Alpine mirrors at " +
                "https://distfiles.alpinelinux.org/distfiles/. The source code of the GPL-licensed packages " +
                "is also published together with appine's source code at $SOURCE.",
            ) + "\n\n",
        )
        for ((p, v, l, o, c) in rows) body.append("$p $v\n    License: $l\n    aports: main/$o at $c\n")
        doas.orNull?.asFile?.let { apk ->
            // .PKGINFO is in the package's second part (three gzip parts concatenated: signature,
            // control information, data)
            val info = tarEntry(apk, ".PKGINFO").orEmpty().lines()
                .mapNotNull { it.split(" = ", limit = 2).takeIf { kv -> kv.size == 2 } }
                .associate { (k, v) -> k to v }
            body.append("\n").append(
                wrap(
                    "The app also includes this package, which it installs when a regular user first becomes the " +
                        "login account:",
                ),
            ).append("\n\n")
            body.append("${info["pkgname"]} ${info["pkgver"]}\n    License: ${info["license"]}\n")
            body.append("    aports: main/${info["origin"]} at ${info["commit"]}\n")
        }
        body.append("\n").append(wrap("The full texts of the GPL 2.0, Apache 2.0 and MPL 2.0 are at the end of this list.")).append('\n')
        return Triple("Alpine Linux $version minirootfs", "${rows.size} packages: ${licenses.joinToString(", ")}", body.toString())
    }

    /** Reads [path] from a tar.gz (ustar; GNU long names and pax headers are skipped). */
    private fun tarEntry(file: File, path: String): String? {
        GZIPInputStream(file.inputStream().buffered()).use { input ->
            val header = ByteArray(512)
            while (true) {
                if (input.readNBytes(header, 0, 512) < 512) return null
                if (header.all { it == 0.toByte() }) return null
                fun field(from: Int, len: Int) = String(header, from, len, Charsets.ISO_8859_1).substringBefore('\u0000')
                val name = field(345, 155).let { if (it.isEmpty()) "" else "$it/" } + field(0, 100)
                val size = field(124, 12).trim().ifEmpty { "0" }.toLong(8)
                val data = ByteArray(size.toInt())
                if (input.readNBytes(data, 0, data.size) < data.size) return null
                input.skipNBytes((512 - size % 512) % 512)
                if (name.removePrefix("./") == path) return String(data, Charsets.UTF_8)
            }
        }
    }

    /**
     * Crates linked into libappine.so: starting from appine-android, following only normal
     * dependencies (not build or test ones); proc macros (thiserror-impl and the like) and what
     * only they use (syn, quote…) run at compile time, are not in the library and do not count.
     */
    @Suppress("UNCHECKED_CAST")
    private fun crates(): List<Map<String, Any?>> {
        val ws = workspace.get().asFile
        val cargo = File(System.getProperty("user.home"), ".cargo/bin/cargo")
            .takeIf { it.canExecute() }?.path ?: "cargo"
        val json = ByteArrayOutputStream()
        exec.exec {
            workingDir = ws
            executable = cargo
            args(
                "metadata", "--format-version", "1", "--locked", "--all-features",
                "--filter-platform", "aarch64-linux-android", "--manifest-path", File(ws, "Cargo.toml").path,
            )
            standardOutput = json
        }
        val meta = JsonSlurper().parseText(json.toString(Charsets.UTF_8)) as Map<String, Any?>
        val packages = (meta["packages"] as List<Map<String, Any?>>).associateBy { it["id"] as String }
        val nodes = ((meta["resolve"] as Map<String, Any?>)["nodes"] as List<Map<String, Any?>>)
            .associateBy { it["id"] as String }
        val members = (meta["workspace_members"] as List<String>).toSet()
        val start = packages.values.first { it["name"] == "appine-android" }["id"] as String
        val seen = mutableSetOf(start)
        val queue = ArrayDeque(listOf(start))
        while (queue.isNotEmpty()) {
            for (dep in nodes[queue.removeFirst()]?.get("deps") as List<Map<String, Any?>>? ?: emptyList()) {
                val kinds = dep["dep_kinds"] as List<Map<String, Any?>>
                if (kinds.none { it["kind"] == null }) continue
                val id = dep["pkg"] as String
                val targets = packages.getValue(id)["targets"] as List<Map<String, Any?>>
                if (targets.any { "proc-macro" in (it["kind"] as List<*>) }) continue
                if (seen.add(id)) queue.add(id)
            }
        }
        return (seen - members).map { packages.getValue(it) }.sortedBy { it["name"] as String }
    }

    /**
     * The license files a crate publishes. When MIT is one of the alternatives in the license
     * (`MIT OR Apache-2.0` and the like), only the MIT one and the ones not tied to a license (the
     * `AND` term, Unicode for example) are attached, and the other alternatives (Apache, Zlib,
     * 0BSD, Unlicense) are not: appine uses these crates under MIT.
     */
    private fun licenseFiles(dir: File, expr: String): List<File> {
        val all = dir.listFiles().orEmpty()
            .filter { it.isFile && LICENSE_FILE.matches(it.name) }
            .sortedBy { it.name }
        if (!Regex("""\bMIT\b""").containsMatchIn(expr) || !(expr.contains(" OR ") || expr.contains("/"))) return all
        val alternatives = listOf("APACHE", "ZLIB", "UNLICENSE", "0BSD", "BSL", "LLVM")
        val kept = all.filter { f ->
            val n = f.name.uppercase()
            "MIT" in n || alternatives.none { it in n }
        }
        return kept.ifEmpty { all }
    }

    companion object {
        const val SOURCE = "https://github.com/mthli/appine"

        /**
         * Wraps our own paragraphs at [width] columns, the way the license texts are laid out (the
         * page keeps the original line breaks and scrolls sideways when they do not fit). Indented
         * paragraphs (lists, quotations) are left alone.
         */
        fun wrap(text: String, width: Int = 72): String = text.split("\n\n").joinToString("\n\n") { para ->
            val lines = para.lines()
            if (lines.any { it.startsWith(" ") } || lines.all { it.length <= width }) return@joinToString para
            val out = StringBuilder()
            var line = StringBuilder()
            for (word in lines.joinToString(" ").split(Regex("\\s+")).filter { it.isNotEmpty() }) {
                if (line.isNotEmpty() && line.length + 1 + word.length > width) {
                    out.append(line).append('\n')
                    line = StringBuilder()
                }
                if (line.isNotEmpty()) line.append(' ')
                line.append(word)
            }
            out.append(line).toString()
        }

        val APPINE_NOTICE = """
            appine
            Copyright (C) 2026 Matthew Lee

            This program is free software: you can redistribute it and/or modify it under the terms of
            the GNU Affero General Public License as published by the Free Software Foundation, either
            version 3 of the License, or (at your option) any later version.

            This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
            without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
            See the GNU Affero General Public License for more details.

            The complete source code of this app is at $SOURCE.
        """.trimIndent()

        val GHOSTTY_NOTICE = """
            The terminal emulator is libghostty-vt from Ghostty 1.4.0 (https://ghostty.org), built with
            Zig 0.17.0 and linked into the app's native library together with what it uses: uucode,
            simdutf, Highway and Wuffs (listed below) and Zig's compiler_rt. Their source code, as
            built, is also published together with appine's source code at $SOURCE. Ghostty's license:
        """.trimIndent()

        val SQLITE_BLESSING = """
            SQLite is built into the app by the libsqlite3-sys crate. Its author disclaims copyright to
            the source code. In place of a legal notice, here is a blessing:

               May you do good and not evil.
               May you find forgiveness for yourself and forgive others.
               May you share freely, never taking more than you give.
        """.trimIndent()

        /** The libraries of these groups are all Apache-2.0. */
        val APACHE_GROUPS = listOf(
            "androidx.",
            "org.jetbrains.kotlin:",
            "org.jetbrains.kotlinx:",
            "org.jetbrains:annotations:",
            "com.google.guava:listenablefuture:",
            "com.google.android.material:",
            "com.google.errorprone:error_prone_annotations:",
            "org.jspecify:",
        )

        val LICENSE_FILE = Regex("""(?i)^(LICEN[CS]E|COPYING|UNLICENSE|NOTICE|COPYRIGHT)([-._ ].*)?$""")
    }
}

val rootfsAsset = tasks.register<RootfsAssetTask>("rootfsAsset") {
    rootfsTarball?.let { tarball.set(it) }
    doasPackage?.let { doas.set(it) }
    outputDir.set(layout.buildDirectory.dir("generated/rootfs/assets"))
}

androidComponents {
    onVariants { variant ->
        val buildRust = tasks.register<CargoNdkTask>("buildRust${variant.name.replaceFirstChar(Char::uppercase)}") {
            workspace.set(repoRoot)
            ndkDirectory.set(androidComponents.sdkComponents.ndkDirectory.map { it.asFile.path })
            features.set(if (variant.buildType == "debug") listOf("test-hooks", "borrow-check") else emptyList())
            outputDir.set(layout.buildDirectory.dir("generated/rust/${variant.name}/jniLibs"))
        }
        variant.sources.jniLibs?.addGeneratedSourceDirectory(buildRust, CargoNdkTask::outputDir)
        variant.sources.assets?.addGeneratedSourceDirectory(rootfsAsset, RootfsAssetTask::outputDir)
        val notices = tasks.register<NoticesTask>("generateNotices${variant.name.replaceFirstChar(Char::uppercase)}") {
            license.set(repoRoot.file("LICENSE"))
            texts.set(layout.projectDirectory.dir("../licenses"))
            rootfsTarball?.let { tarball.set(it) }
            doasPackage?.let { doas.set(it) }
            cargoLock.set(repoRoot.file("Cargo.lock"))
            workspace.set(repoRoot)
            // Modules in the dependency graph (not in-project modules), resolving only the graph
            // without downloading
            androidLibraries.set(
                variant.runtimeConfiguration.incoming.resolutionResult.rootComponent.map { top ->
                    val seen = mutableSetOf<ResolvedComponentResult>()
                    val modules = sortedSetOf<String>()
                    val queue = ArrayDeque(listOf(top))
                    while (queue.isNotEmpty()) {
                        val c = queue.removeFirst()
                        if (!seen.add(c)) continue
                        (c.id as? ModuleComponentIdentifier)?.let { modules.add("${it.group}:${it.module}:${it.version}") }
                        c.dependencies.filterIsInstance<ResolvedDependencyResult>().forEach { queue.add(it.selected) }
                    }
                    modules.toList()
                },
            )
            outputDir.set(layout.buildDirectory.dir("generated/notices/${variant.name}/assets"))
        }
        variant.sources.assets?.addGeneratedSourceDirectory(notices, NoticesTask::outputDir)
    }
}

dependencies {
    implementation("androidx.core:core:1.19.1")
    // Material 3 for Settings and the license pages (brings AppCompat, which their activities
    // extend, and androidx.activity for enableEdgeToEdge)
    implementation("com.google.android.material:material:1.14.0")
    androidTestImplementation("androidx.test:runner:1.7.0")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
}
