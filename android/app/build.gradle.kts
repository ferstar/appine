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

/** 仓库根目录：Cargo 工作区和 `scripts/fetch-rootfs` 下载的 rootfs 都在这里。 */
val repoRoot: Directory = rootProject.layout.projectDirectory.dir("..")

/** Alpine minirootfs：`scripts/fetch-rootfs` 下载并校验过的 tarball。 */
val rootfsTarball: File? = repoRoot.dir("target/rootfs").asFile
    .listFiles { f -> f.name.matches(Regex("alpine-minirootfs-.*-aarch64\\.tar\\.gz")) }
    ?.maxByOrNull { it.name }

/** 例如 3.24.3 → v3.24，写进 apk 的仓库地址。 */
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
            // 只在本地安装：用调试密钥签名
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        // 原生库不压缩、按页对齐地放在 APK 里，直接从 APK 映射（16K 页设备也一样）
        jniLibs.useLegacyPackaging = false
    }
}

/**
 * 用 cargo-ndk 构建 libappine.so（hosts/android）。Rust 部分总是 release 构建：调试构建的解释器太慢。
 * debug 版 App 打开 `test-hooks` 特性，instrumentation 测试用测试钩子在 App 进程里模拟内核层和
 * 引擎的 bug（只在设置了 `APPINE_TEST_HOOKS=1` 时生效）；也打开 `borrow-check`，放开 BKL 时检查
 * 没有借着内核对象。
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
        // cargo 自己做增量构建
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
            // 两个都设：只设一个时，环境里另一个指向别的 NDK（GitHub 的 runner 预设了
            // ANDROID_NDK_ROOT）cargo-ndk 会警告，用哪个取决于它的优先顺序
            environment("ANDROID_NDK_HOME", ndkDirectory.get())
            environment("ANDROID_NDK_ROOT", ndkDirectory.get())
            environment("CARGO_TARGET_DIR", File(root, "target/android").path)
        }
    }
}

/** 把 rootfs tarball 放进 assets（首次启动时导入，设计文档 §11.4）。 */
abstract class RootfsAssetTask : DefaultTask() {
    @get:InputFile
    @get:Optional
    abstract val tarball: RegularFileProperty

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
    }
}

/**
 * 生成设置里"开源许可"页的内容（设计文档 §13.4），放在 assets 的 `notices/`：`index.tsv` 每行是
 * "标题<TAB>说明<TAB>文件名"，各个文件是纯文本。依次是 appine 自己（AGPL）、vendored 的 Termux
 * 终端库、APK 里的 Alpine minirootfs 的各个包、Android 库、链接进 libappine.so 的 Rust crate（各自
 * 发布的许可证文件原样附上）、SQLite，最后是几份许可证全文（`android/licenses/`）。
 *
 * 新加的 Android 库不在下面认识的组里时构建失败：先确认它的许可证与 AGPL-3.0 兼容，再加进来。
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
    abstract val cargoLock: RegularFileProperty

    /** APK 里的 Android 库，`组:名字:版本`。 */
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
        entry(
            "Termux terminal-emulator, terminal-view",
            "GPL-3.0-only; parts Apache-2.0 (Terminal Emulator for Android)",
            "termux.txt",
            wrap(TERMUX_NOTICE) + "\n\n" + text("termux-app-LICENSE.md") +
                "\n\nNOTICE of Terminal Emulator for Android:\n\n" + text("jackpal-NOTICE.txt"),
        )
        alpine()?.let { (title, subtitle, body) -> entry(title, subtitle, "alpine.txt", body) }

        val libraries = androidLibraries.get()
        for (lib in libraries) {
            if (APACHE_GROUPS.none { lib.startsWith(it) }) {
                throw GradleException("notices: unknown license for Android library $lib; check it and add it to APACHE_GROUPS")
            }
        }
        entry(
            "Android libraries", "Apache-2.0 · AndroidX, Kotlin, kotlinx.coroutines and others", "android.txt",
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

    /** APK 里的 Alpine minirootfs：标题、说明、正文（各个包的版本、许可证、aports 的提交）。 */
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
        body.append("\n").append(wrap("The full texts of the GPL 2.0, Apache 2.0 and MPL 2.0 are at the end of this list.")).append('\n')
        return Triple("Alpine Linux $version minirootfs", "${rows.size} packages: ${licenses.joinToString(", ")}", body.toString())
    }

    /** 从 tar.gz 里读出 [path] 的内容（ustar；GNU 的长文件名、pax 头跳过）。 */
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
     * 链接进 libappine.so 的 crate：从 appine-android 出发，只走普通依赖（不含构建、测试用的），
     * 过程宏（thiserror-impl 这些）和只有它们用到的（syn、quote……）编译时运行，不在库里，不算。
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
     * crate 发布时带的许可证文件。许可证里 MIT 是可选的一项时（`MIT OR Apache-2.0` 这类）只附 MIT
     * 的和不分许可证的（`AND` 的那一项，比如 Unicode），别的可选项（Apache、Zlib、0BSD、Unlicense）
     * 不附：appine 按 MIT 使用它们。
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
         * 把自己写的段落按 [width] 列折行，与许可证原文的排法一样（页面上保持原来的换行，放不下时
         * 横向滚动）。有缩进的段落（列表、引文）不动。
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

        val TERMUX_NOTICE = """
            The terminal-emulator and terminal-view libraries come from termux-app
            (https://github.com/termux/termux-app) at commit 8629e632fcb95da272221be327db653fb24befe9,
            with the changes described in android/README.md of appine's source code. termux-app is
            licensed under the GPL 3.0 only; parts of these two libraries come from Terminal Emulator for
            Android (https://github.com/jackpal/Android-Terminal-Emulator) under the Apache License 2.0.
            The full texts of both licenses are at the end of this list. termux-app's license statement:
        """.trimIndent()

        val SQLITE_BLESSING = """
            SQLite is built into the app by the libsqlite3-sys crate. Its author disclaims copyright to
            the source code. In place of a legal notice, here is a blessing:

               May you do good and not evil.
               May you find forgiveness for yourself and forgive others.
               May you share freely, never taking more than you give.
        """.trimIndent()

        /** 这些组的库都是 Apache-2.0。 */
        val APACHE_GROUPS = listOf(
            "androidx.",
            "org.jetbrains.kotlin:",
            "org.jetbrains.kotlinx:",
            "org.jetbrains:annotations:",
            "com.google.guava:listenablefuture:",
            "org.jspecify:",
        )

        val LICENSE_FILE = Regex("""(?i)^(LICEN[CS]E|COPYING|UNLICENSE|NOTICE|COPYRIGHT)([-._ ].*)?$""")
    }
}

val rootfsAsset = tasks.register<RootfsAssetTask>("rootfsAsset") {
    rootfsTarball?.let { tarball.set(it) }
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
            cargoLock.set(repoRoot.file("Cargo.lock"))
            workspace.set(repoRoot)
            // 依赖图里的模块（不含 terminal-view 这些工程内的模块），只解析图、不下载
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
    implementation(project(":terminal-view"))
    implementation("androidx.core:core:1.19.1")
    androidTestImplementation("androidx.test:runner:1.7.0")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
}
