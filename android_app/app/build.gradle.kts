import java.util.Properties

plugins {
    id("com.android.application")
}

// Version lives in ../version.properties so build_android.ps1 -Release can bump
// the code without editing this file.
val versionProps = Properties().apply {
    val f = rootProject.file("version.properties")
    if (f.exists()) f.inputStream().use { load(it) }
}
val appVersionCode = (versionProps.getProperty("versionCode") ?: "1").trim().toInt()
val appVersionName = (versionProps.getProperty("versionName") ?: "0.1.0").trim()

// ── release signing ─────────────────────────────────────────────────────────
// An unsigned APK cannot be installed. The keystore and its passwords are
// SECRETS and are read from outside the repo, first complete set wins:
//
//   1. environment:  CS_KEYSTORE, CS_KEYSTORE_PASSWORD, CS_KEY_ALIAS,
//                    CS_KEY_PASSWORD
//   2. gradle properties in %USERPROFILE%\.gradle\gradle.properties (NOT here):
//                    cs.keystore, cs.keystorePassword, cs.keyAlias, cs.keyPassword
//
// With neither, a release build still succeeds but is unsigned, and says so.
fun secret(env: String, prop: String): String? =
    (System.getenv(env) ?: providers.gradleProperty(prop).orNull)?.takeIf { it.isNotBlank() }

val ksPath = secret("CS_KEYSTORE", "cs.keystore")
val ksPass = secret("CS_KEYSTORE_PASSWORD", "cs.keystorePassword")
val ksAlias = secret("CS_KEY_ALIAS", "cs.keyAlias")
val ksKeyPass = secret("CS_KEY_PASSWORD", "cs.keyPassword")
val ksFile = ksPath?.let { file(it) }
val canSign = ksFile != null && ksFile.exists() && ksPass != null && ksAlias != null && ksKeyPass != null

android {
    namespace = "kr.chronos.sword"
    compileSdk = 35
    ndkVersion = "25.1.8937393"

    defaultConfig {
        // Deliberately NOT co.kr.eamobile.chronosMian: a different applicationId
        // means this can be installed alongside the original 32-bit APK rather
        // than replacing it, and it keeps its own /data/data directory (and so
        // its own save file).
        applicationId = "kr.chronos.sword"
        // API 26 is where AAudio and the modern window insets APIs start, and it
        // is far below any device that would need this build in the first place
        // — the reason this port exists is 64-bit-only hardware.
        minSdk = 26
        targetSdk = 35
        versionCode = appVersionCode
        versionName = appVersionName
        ndk {
            // The whole point: one ABI, and it is the 64-bit one. The original
            // shipped lib/armeabi (ARMv5) and nothing else, which is exactly why
            // it will not start on a device without AArch32 support.
            abiFilters += "arm64-v8a"
        }
    }

    if (canSign) {
        signingConfigs {
            create("release") {
                storeFile = ksFile
                storePassword = ksPass
                keyAlias = ksAlias
                keyPassword = ksKeyPass
                enableV1Signing = true
                enableV2Signing = true
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            if (canSign) {
                signingConfig = signingConfigs.getByName("release")
            }
            // `-PcsDebuggable=true` makes a release-SIGNED but debuggable APK.
            //
            // The point is `run-as`, which is the only way to reach
            // /data/data/kr.chronos.sword/files on an unrooted phone — and that
            // directory holds `kickass`, the save. Without it a lost save can
            // only be restored through whatever in-game path happens to exist,
            // and on 2026-08-16 that path turned out to be one-way: 데이터 관리
            // uploads (and then deletes the local save), but the download half
            // is not reachable from this build's menu.
            //
            // Off by default, and deliberately a flag rather than an edit, so a
            // normal `-Release` can never ship debuggable by accident. Same
            // signing key either way, so it installs over the normal build and
            // the save survives the swap in both directions.
            if (project.findProperty("csDebuggable") == "true") {
                isDebuggable = true
                logger.lifecycle("==> DEBUGGABLE release build (run-as enabled). Do not ship this.")
            }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    androidResources {
        // The guest binary is read once at startup and the VFS is read
        // repeatedly by offset; leaving both uncompressed lets the AssetManager
        // hand them over without inflating 21 MB first. data.vfs.mp3 is already
        // on AAPT's default no-compress list by extension — this makes it
        // explicit and adds the guest image.
        noCompress += listOf("so", "mp3")
    }

    packaging {
        jniLibs {
            // cargo-ndk writes the already-stripped cdylib into
            // src/main/jniLibs; leave it as AGP found it.
            keepDebugSymbols += "**/*.so"
        }
    }
}

// games-activity 4.4.0 drags in the pre-1.8 split kotlin-stdlib-jdk7/jdk8
// (1.6.21) artifacts, whose classes were folded into the main kotlin-stdlib in
// 1.8+. That collides with appcompat 1.7.0's kotlin-stdlib 1.8.22 (duplicate
// class). Forcing the split artifacts to 1.8.22 (now empty stubs) resolves it.
configurations.all {
    resolutionStrategy {
        force(
            "org.jetbrains.kotlin:kotlin-stdlib-jdk7:1.8.22",
            "org.jetbrains.kotlin:kotlin-stdlib-jdk8:1.8.22",
        )
    }
}

if (gradle.startParameter.taskNames.any { it.contains("Release", ignoreCase = true) }) {
    if (canSign) {
        logger.lifecycle("==> release signing: ${ksFile!!.absolutePath} (alias $ksAlias), versionCode $appVersionCode")
    } else {
        logger.warn(
            "==> WARNING: release build will be UNSIGNED and CANNOT BE INSTALLED.\n" +
                "    Set CS_KEYSTORE / CS_KEYSTORE_PASSWORD / CS_KEY_ALIAS / CS_KEY_PASSWORD,\n" +
                "    or cs.keystore / cs.keystorePassword / cs.keyAlias / cs.keyPassword in\n" +
                "    %USERPROFILE%\\.gradle\\gradle.properties. See build_android.ps1 -NewKeystore."
        )
    }
}

dependencies {
    // Provides com.google.androidgamesdk.GameActivity, whose native methods are
    // implemented inside libmain.so (android-activity 0.6.1 bundles the matching
    // GameActivity 4.4.0 C++).
    implementation("androidx.games:games-activity:4.4.0")
    // GameActivity extends androidx.appcompat.app.AppCompatActivity.
    implementation("androidx.appcompat:appcompat:1.7.0")
}
