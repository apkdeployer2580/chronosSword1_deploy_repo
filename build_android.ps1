# Build the ChronosSword 64-bit APK.
#
#   .\build_android.ps1                  debug APK
#   .\build_android.ps1 -Install         ... and push it to the attached device
#   .\build_android.ps1 -Release         signed release (see the keystore notes
#                                        in android_app/app/build.gradle.kts)
#   .\build_android.ps1 -NewKeystore     create a release keystore, print how to
#                                        wire it up, and stop
#
# What it does, in order: build the dlltool shim this host needs, cross-compile
# cs_android to aarch64, refresh the bundled payload in assets/, then gradle.

param(
    [switch]$Release,
    [switch]$Install,
    [switch]$NewKeystore,
    [switch]$SkipRust,
    # adb serial. Required when more than one device is attached, which is the
    # normal state on a bench with a phone and a tablet plugged in.
    [string]$Device,
    # Follow the app's logcat after installing.
    [switch]$Logcat,
    # Release-optimised Rust inside a debug-signed APK. `-Release` produces an
    # UNSIGNED apk unless a keystore is configured, and a plain debug build
    # compiles the interpreter without LTO — on a phone that is roughly half
    # speed, so it is not a number worth quoting. This is the combination to
    # measure with and to hand to a tester.
    [switch]$Fast,
    # Release-signed but DEBUGGABLE, which is what enables `adb shell run-as` —
    # the only way to reach /data/data/kr.chronos.sword/files on an unrooted
    # phone, and therefore the only way to inspect or restore `kickass` (the
    # save) from here. Same key as a normal release, so it installs over one and
    # back again without uninstalling, which means the save survives both swaps.
    # Never leave a build like this on the phone.
    [switch]$Debuggable,
    # Report the player's chosen ID instead of the minted phone number — i.e.
    # `CS_ID_AS_PHONE`. **On by default, and baked HERE rather than left to the
    # shell**, because that is exactly how versionCode 68/69 shipped with the
    # whole ID feature dead: the knob's compile default is 0 (deliberately — the
    # server had to be deployed before a non-numeric identity could go on the
    # wire, LETTER_ID.md R2), that gate has been closed since 2026-08-17, and
    # v62..v67 only carried the 1 because whoever built them happened to have
    # the variable exported. Nothing in the repo recorded it, so the first build
    # from a clean shell silently turned the 「내 ID」 tile, the ID keyboard and
    # the identity itself back off — and cs_core/build.rs's rerun-if-env-changed
    # made sure the crate really was recompiled that way rather than reusing the
    # good object file. Same class as the versionCode 34 server address, and the
    # same fix: the repo holds the value, not a person.
    [switch]$NoIdAsPhone,
    # Which cs_server the built app dials, as host or host:port. **Defaults to
    # `off` in this snapshot**: it carries no server, and a build must never
    # silently dial someone else's machine.
    #
    # This has to be decided HERE because `redirect_target` (cs_core/src/net.rs)
    # reads it through `option_env!` — i.e. at compile time. Its three sources
    # are, in order: the environment at runtime (an APK has none), the value
    # baked in at build time (this), and a loopback fallback that only exists in
    # debug builds. A release build with none of the three does not redirect at
    # all, and the unpatched guest then dials its own hardcoded `210.222.18.28`,
    # which died with the original service. That is exactly what shipped as
    # versionCode 34: no address in the binary, so every network feature —
    # 편지 included — failed to connect.
    #
    # `-Server off` bakes nothing, which restores that no-redirect behaviour on
    # purpose. Keep it explicit either way; do not let a default appear inside
    # cs_core, so that a build never silently dials someone else's machine.
    [string]$Server = 'off'
)

# NB: deliberately NOT 'Stop'. cargo, gradle and adb all write progress to
# stderr, and with 'Stop' PowerShell turns the first such line into a
# terminating NativeCommandError — the build dies on "Building arm64-v8a". Every
# native call below is checked with $LASTEXITCODE instead, which is the thing
# that actually reports failure.
$ErrorActionPreference = 'Continue'
$root = $PSScriptRoot

# --- toolchain locations ----------------------------------------------------
$Sdk = @(
    $env:ANDROID_HOME
    $env:ANDROID_SDK_ROOT
    "$env:LOCALAPPDATA\Android\Sdk"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $Sdk) { throw 'Android SDK not found. Set $env:ANDROID_HOME.' }

$NdkVersion = '25.1.8937393'
$Ndk = Join-Path $Sdk "ndk\$NdkVersion"
if (-not (Test-Path $Ndk)) {
    $Ndk = Get-ChildItem (Join-Path $Sdk 'ndk') -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending | Select-Object -First 1 -ExpandProperty FullName
}
if (-not $Ndk) { throw "Android NDK not found under $Sdk\ndk." }
$NdkBin = Join-Path $Ndk 'toolchains\llvm\prebuilt\windows-x86_64\bin'

$env:ANDROID_HOME = $Sdk
$env:ANDROID_SDK_ROOT = $Sdk
$env:ANDROID_NDK_HOME = $Ndk

# AGP 8.7 needs a JDK 17+ to even resolve its own plugin. Whatever `java` is
# first on PATH is not necessarily that (on this host it is a Java 8 JRE), and
# the failure it produces — "Dependency requires at least JVM runtime version
# 11" while configuring the root project — does not name JAVA_HOME, so pick the
# JDK explicitly.
if (-not $env:JAVA_HOME -or -not (Test-Path (Join-Path $env:JAVA_HOME 'bin\javac.exe'))) {
    $jdk = @(
        Get-ChildItem 'C:\Program Files\Eclipse Adoptium' -Directory -ErrorAction SilentlyContinue
        Get-ChildItem 'C:\Program Files\Java' -Directory -ErrorAction SilentlyContinue
        Get-ChildItem 'C:\Program Files\Microsoft' -Directory -Filter 'jdk*' -ErrorAction SilentlyContinue
    ) | Where-Object { Test-Path (Join-Path $_.FullName 'bin\javac.exe') } |
        Sort-Object Name -Descending | Select-Object -First 1
    if (-not $jdk) { throw 'No JDK 17+ found. Set $env:JAVA_HOME to one.' }
    $env:JAVA_HOME = $jdk.FullName
}
Write-Host "JAVA_HOME = $env:JAVA_HOME"

$Adb = @(
    $env:CS_ADB
    (Join-Path $Sdk 'platform-tools\adb.exe')
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if ($Install -and -not $Adb) { throw 'adb not found. Set $env:CS_ADB.' }

# --- keystore bootstrap -----------------------------------------------------
if ($NewKeystore) {
    $ks = Join-Path $HOME 'chronos_sword_release.jks'
    if (Test-Path $ks) { throw "$ks already exists; refusing to overwrite a signing key." }
    $keytool = Join-Path $env:JAVA_HOME 'bin\keytool.exe'
    if (-not (Test-Path $keytool)) { $keytool = 'keytool' }
    & $keytool -genkeypair -v -keystore $ks -alias chronos -keyalg RSA -keysize 4096 -validity 10000
    if ($LASTEXITCODE -ne 0) { throw "keytool failed ($LASTEXITCODE)" }
    Write-Host ''
    Write-Host "Keystore written to $ks. Add to %USERPROFILE%\.gradle\gradle.properties:"
    Write-Host "  cs.keystore=$($ks -replace '\\','/')"
    Write-Host '  cs.keystorePassword=<the store password you just typed>'
    Write-Host '  cs.keyAlias=chronos'
    Write-Host '  cs.keyPassword=<the key password you just typed>'
    Write-Host ''
    Write-Host 'KEEP THIS FILE AND THESE PASSWORDS. An app can never be updated on Play with a different key.'
    return
}

# --- dlltool shim -----------------------------------------------------------
# Cross-compiling to android from a windows-gnu host still builds HOST
# build-deps (cc -> windows-sys, getrandom) whose raw-dylib imports rustc
# materialises by shelling out to `dlltool`. The GNU dlltool rustup ships in
# `self-contained` needs an `as` the toolchain does not include, and a renamed
# llvm-dlltool is handed GNU-only flags it rejects. So: a real wrapper.
# `target.<host>.rustflags` does not reach build-deps, which is why this is a
# PATH shim rather than a config setting. See tools/dlltool_shim.rs.
$Shim = Join-Path $env:TEMP 'cs-dlltool'
New-Item -ItemType Directory -Force $Shim | Out-Null
$RealDlltool = Join-Path $Shim 'real-llvm-dlltool.exe'
$llvmDlltool = "$HOME\.rustup\toolchains\stable-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin\llvm-dlltool.exe"
if (Test-Path $llvmDlltool) {
    Copy-Item $llvmDlltool $RealDlltool -Force
} elseif (Test-Path "$NdkBin\llvm-ar.exe") {
    # llvm-ar/llvm-lib/llvm-ranlib/llvm-dlltool are ONE multicall binary that
    # picks its personality from argv[0], so a copy named llvm-dlltool IS
    # llvm-dlltool. It loads its DLLs from the NDK bin dir, hence $NdkBin on
    # PATH below.
    Copy-Item "$NdkBin\llvm-ar.exe" $RealDlltool -Force
} else {
    throw "No llvm-dlltool available (looked in the rustup gnu toolchain and $NdkBin)."
}
& "$HOME\.cargo\bin\rustc.exe" -O --edition 2021 (Join-Path $root 'tools\dlltool_shim.rs') -o (Join-Path $Shim 'dlltool.exe')
if ($LASTEXITCODE -ne 0) { throw "could not build the dlltool shim ($LASTEXITCODE)" }
$env:CS_REAL_DLLTOOL = $RealDlltool
# $NdkBin goes LAST so the NDK's cross tools cannot shadow host ones; it is on
# PATH only so the multicall llvm binary can find its own DLLs.
$env:PATH = "$Shim;$HOME\.cargo\bin;$env:JAVA_HOME\bin;$env:PATH;$NdkBin"

# --- payload ----------------------------------------------------------------
# The guest binary and its asset archive are the licensed game data. They are
# copied from payload/ rather than committed under android_app, so a checkout
# without them fails here — loudly — instead of producing an APK that installs,
# launches, and dies on the first asset read.
$Assets = Join-Path $root 'android_app\app\src\main\assets'
New-Item -ItemType Directory -Force $Assets | Out-Null
foreach ($f in @('libchronosMian.so', 'data.vfs.mp3')) {
    $src = Join-Path $root "payload\$f"
    if (-not (Test-Path $src)) {
        throw "payload\$f is missing. Extract it from the original APK (lib/armeabi/ and assets/)."
    }
    Copy-Item $src (Join-Path $Assets $f) -Force
}

# --- identity knob ----------------------------------------------------------
# Same `option_env!` mechanism as the server address below, same trap.
$env:CS_ID_AS_PHONE = if ($NoIdAsPhone) { '0' } else { '1' }
Write-Host "identity: baking CS_ID_AS_PHONE=$($env:CS_ID_AS_PHONE)"

# --- server address ---------------------------------------------------------
# Must be set before cargo, because cs_core reads it with `option_env!`.
# cs_core/build.rs is what makes cargo notice a change here; without it the
# crate keeps its previous object file and the old address (or none) survives
# the rebuild, which is the trap `cargo clean -p cs_core` used to work around.
if ($Server -and $Server -ne 'off') {
    $parts = $Server -split ':', 2
    $env:CS_SERVER_HOST = $parts[0]
    $env:CS_SERVER_PORT = if ($parts.Count -gt 1 -and $parts[1]) { $parts[1] } else { '8094' }
    Write-Host "server: baking $($env:CS_SERVER_HOST):$($env:CS_SERVER_PORT)"
} else {
    # Removed, not blanked: this shell's own CS_SERVER_HOST would otherwise be
    # inherited and quietly win over -Server off.
    Remove-Item Env:CS_SERVER_HOST -ErrorAction SilentlyContinue
    Remove-Item Env:CS_SERVER_PORT -ErrorAction SilentlyContinue
    Write-Host 'server: -Server off, nothing baked (the guest dials its original dead host)'
}

# --- rust -------------------------------------------------------------------
$JniLibs = Join-Path $root 'android_app\app\src\main\jniLibs'
if (-not $SkipRust) {
    Push-Location $root
    try {
        # Build one flat argument list. NB: do not name it `$profile` — that is a
        # PowerShell automatic variable (the path to the user's profile script),
        # and splatting it feeds cargo a stray path argument.
        $cargoArgs = @('ndk', '-t', 'arm64-v8a', '--platform', '26', '-o', $JniLibs, 'build')
        if ($Release -or $Fast) { $cargoArgs += '--release' }
        $cargoArgs += @('-p', 'cs_android')
        Write-Host "cargo $($cargoArgs -join ' ')"
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) { throw "cargo ndk failed ($LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
}
$so = Join-Path $JniLibs 'arm64-v8a\libmain.so'
if (-not (Test-Path $so)) { throw "expected $so to exist after the rust build." }
Write-Host ("libmain.so: {0:N1} MB" -f ((Get-Item $so).Length / 1MB))

# --- version bump (release only) --------------------------------------------
# `$VersionCode` outlives this block: the hand-off copy at the bottom names the
# APK after it, and a debug build has to read the number without bumping it.
$VersionFile = Join-Path $root 'android_app\version.properties'
$VersionCode = $null
if (Test-Path $VersionFile) {
    $lines = Get-Content $VersionFile
    $code = [int](($lines | Where-Object { $_ -match '^versionCode=' }) -replace 'versionCode=', '')
    if ($Release) {
        $code++
        $lines = $lines | ForEach-Object { if ($_ -match '^versionCode=') { "versionCode=$code" } else { $_ } }
        Set-Content $VersionFile $lines -Encoding ascii
        Write-Host "versionCode -> $code"
    }
    $VersionCode = $code
}

# --- gradle -----------------------------------------------------------------
Push-Location (Join-Path $root 'android_app')
try {
    $task = if ($Release) { 'assembleRelease' } else { 'assembleDebug' }
    $gradleArgs = @($task, '--console=plain')
    if ($Debuggable) { $gradleArgs += '-PcsDebuggable=true' }
    & .\gradlew.bat @gradleArgs
    if ($LASTEXITCODE -ne 0) { throw "gradle $task failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
}

$outDir = Join-Path $root ('android_app\app\build\outputs\apk\' + $(if ($Release) { 'release' } else { 'debug' }))
$apk = Get-ChildItem $outDir -Filter '*.apk' | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $apk) { throw "no APK found in $outDir" }
Write-Host ("APK: {0} ({1:N1} MB)" -f $apk.FullName, ($apk.Length / 1MB))

# --- hand-off copy ----------------------------------------------------------
# gradle always writes `app-release.apk`, so two builds in one folder are
# indistinguishable — and the versionCode is what everything else identifies a
# build by: the phone's app info, `aapt2 dump badging`, and every report in
# CLAUDE.md. Stage a copy carrying that number. `dist/` is gitignored; these are
# 27 MB binaries, not source.
if ($VersionCode) {
    $dist = Join-Path $root 'dist'
    New-Item -ItemType Directory -Force $dist | Out-Null
    $suffix = if ($Release) { '' } else { '_debug' }
    $staged = Join-Path $dist ('chronosSword1_v0.{0}{1}.apk' -f $VersionCode, $suffix)
    Copy-Item $apk.FullName $staged -Force
    Write-Host ("staged: {0}" -f $staged)
} else {
    Write-Warning 'version.properties has no versionCode; skipping the dist copy.'
}

if ($Install) {
    $sel = if ($Device) { @('-s', $Device) } else { @() }
    & $Adb @sel install -r $apk.FullName
    if ($LASTEXITCODE -ne 0) { throw "adb install failed ($LASTEXITCODE)" }
    & $Adb @sel shell am force-stop kr.chronos.sword
    & $Adb @sel logcat -c
    & $Adb @sel shell am start -n 'kr.chronos.sword/.MainActivity'
    Write-Host 'Installed and launched.'
    if ($Logcat) { & $Adb @sel logcat -s chronos:V AndroidRuntime:E }
    else { Write-Host "Logs: adb $sel logcat -s chronos" }
}
