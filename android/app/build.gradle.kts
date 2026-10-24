import java.io.File
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

/** 用 cargo-ndk 构建 libappine.so（hosts/android）。Rust 部分总是 release 构建：调试构建的解释器太慢。 */
abstract class CargoNdkTask : DefaultTask() {
    @get:Internal
    abstract val workspace: DirectoryProperty

    @get:Input
    abstract val ndkDirectory: Property<String>

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
            environment("ANDROID_NDK_HOME", ndkDirectory.get())
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

val buildRust = tasks.register<CargoNdkTask>("buildRust") {
    workspace.set(repoRoot)
    ndkDirectory.set(androidComponents.sdkComponents.ndkDirectory.map { it.asFile.path })
    outputDir.set(layout.buildDirectory.dir("generated/rust/jniLibs"))
}

val rootfsAsset = tasks.register<RootfsAssetTask>("rootfsAsset") {
    rootfsTarball?.let { tarball.set(it) }
    outputDir.set(layout.buildDirectory.dir("generated/rootfs/assets"))
}

androidComponents {
    onVariants { variant ->
        variant.sources.jniLibs?.addGeneratedSourceDirectory(buildRust, CargoNdkTask::outputDir)
        variant.sources.assets?.addGeneratedSourceDirectory(rootfsAsset, RootfsAssetTask::outputDir)
    }
}

dependencies {
    implementation(project(":terminal-view"))
    implementation("androidx.core:core:1.19.1")
    androidTestImplementation("androidx.test:runner:1.7.0")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
}
