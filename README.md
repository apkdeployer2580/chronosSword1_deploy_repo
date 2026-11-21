# ChronosSword — 64비트 네이티브 앱

피처폰 WIPI-C RPG **크로노스 소드**를 64비트 전용 안드로이드 기기에서 돌리는
네이티브 앱. 원본 `libchronosMian.so`(ARMv5TE, 887 KB) 를 **한 바이트도 고치지 않고**
AArch64 프로세스 안에서 해석 실행한다.

원본 APK 를 64비트로 "변환"할 수 없는 이유와, 대신 이 구조를 고른 근거는
[`ANALYSIS.md`](ANALYSIS.md) 에 있다.

이 저장소는 작업 저장소의 스냅샷이다. 소스 주석과 문서가 가리키는
`CLAUDE.md`(작업 일지), `LETTER_ID.md`(편지 ID 설계와 서버 운영 기록),
`cs_server/`(사설 서버)는 여기에 없다.

```
  android_app (arm64-v8a APK)              cs_probe (헤드리스, PC)
                    \                     /
                     +----- cs_core -----+
                     |  Vm: CPU·메모리   |
                     |  libc/GL/JNI HLE  |
                     +---------+---------+
                               | ARMv5TE 해석
                     libchronosMian.so  (원본 그대로)
```

## 준비물

`payload/` 에 원본 APK 에서 꺼낸 두 파일이 **들어 있다** — 받은 그대로 빌드된다:

```
payload/libchronosMian.so     원본 APK 의 lib/armeabi/libchronosMian.so
payload/data.vfs.mp3          원본 APK 의 assets/data.vfs.mp3
```

`chronosSword/` 에는 원본 배포본(`chronosSword.zip`)과 옆에 놓고 비교하는 원본
32비트 APK 가 있다. 빌드에는 쓰이지 않는다.

빌드에 필요한 것 (Windows): Android SDK · NDK 25.1.8937393 · JDK 17+ ·
Rust (`rustup target add aarch64-linux-android`, `cargo install cargo-ndk`).

## 빌드

```powershell
.\build_android.ps1                 # 디버그 APK
.\build_android.ps1 -Install        # 빌드 + 기기에 설치·실행
.\build_android.ps1 -Release        # 서명된 릴리스 (키스토어는 아래)
.\build_android.ps1 -NewKeystore    # 릴리스 키스토어 생성 방법 안내
```

산출물: `android_app/app/build/outputs/apk/…/*.apk` — 그리고 **나눠 줄 사본이
`dist/chronosSword1_v0.<versionCode>.apk`** 로 같이 놓인다(디버그 빌드는
`_debug` 접미). gradle 은 무엇을 빌드해도 `app-release.apk` 한 이름을 쓰므로
파일만 보고는 어느 빌드인지 알 수 없어서다. `dist/` 는 커밋하지 않는다.
릴리스 기준 **25 MB**, `native-code: 'arm64-v8a'` 하나뿐이다.

기기 로그: `adb logcat -s chronos`

### 릴리스 서명

`-Release` 는 `%USERPROFILE%\.gradle\gradle.properties`(저장소 바깥)의 네 값으로
서명한다(`cs.keystore` / `cs.keystorePassword` / `cs.keyAlias` / `cs.keyPassword`).
키스토어가 없으면 `-NewKeystore` 가 만들고 연결 방법을 알려 준다. 키스토어와
암호는 저장소에 넣지 않는다.

```powershell
.\build_android.ps1 -Release        # version.properties 의 versionCode 자동 증가

# 나온 APK 가 정말 그 키로 서명됐는지
& "$env:LOCALAPPDATA\Android\Sdk\build-tools\36.0.0\apksigner.bat" verify --print-certs `
    dist\chronosSword1_v0.80.apk
#  -> 공식 배포본이면 SHA-256 4b98e6349d3b3d31ad4ca6fdce90045468c2d85e2ca79e360a831893c39f0c47
```

**배포용은 반드시 `-Release` 로 굽는다.** 디버그 APK 는 머신마다 다른
`~/.android/debug.keystore` 로 서명되므로, 그것을 배포하면 다음 업데이트가
설치되지 않고 사용자는 uninstall(= 세이브 삭제) 외에 방법이 없다. 같은 이유로
기기에 디버그 서명본이 깔려 있으면 릴리스본을 덮어쓸 수 없다 — 한 번은 지워야
한다.

기기에서 업데이트가 제대로 됐는지는 `firstInstallTime` 이 그대로인지로 본다:

```powershell
adb shell "dumpsys package kr.chronos.sword | grep -E 'versionCode|InstallTime|UpdateTime'"
```

## 개발 루프 — 헤드리스 프로브

기기 없이, 창 없이, 결정적으로 게스트를 돌린다. 안드로이드 앱과 **같은**
`cs_core` 를 쓰므로 여기서 재현되는 문제는 기기에서도 같은 문제다.

```powershell
cargo run --release -p cs_probe -- --frames 600 --ms 33 `
    --keys 210:LEFT,230:OK,320:OK --dump 1,200,400,600 --out out
```

* `--ms` 는 사실상 필수다. 게스트는 WIPI 타이머로 idle 하므로 가상 시계를
  돌리지 않으면 화면이 검은 채로 멈춘다.
* `--profile` 은 게스트 PC 를 샘플링해 **어느 게스트 함수가 느린지**를 이름으로
  보여 준다. 다음 최적화를 고를 때 쓰는 도구다.
* `--profile-from N --profile-to M` 은 그 샘플링을 **프레임 구간으로 좁힌다**.
  전체 프로파일은 실행이 시간을 가장 많이 쓴 곳 — 메뉴를 걸어서 게임까지 가는
  시나리오라면 메뉴 — 에 눌려서 "그 프레임에서 뭐가 비쌌나"에 답하지 못한다.
  아래 프레임 비용 보고가 지목한 번호를 그대로 넣으면 답이 나온다.
* 매 실행 끝에 **프레임 비용 분포**(평균/p50/p90/p99/최악, 기기 46 M inst/s 환산
  ms, 33 ms 예산 초과 프레임 수)와 **가장 비싼 프레임 번호**가 찍힌다. "렉이
  걸린다"는 증상은 평균에 안 나타나므로 이쪽을 봐야 하고, 번호를 알면 다음 명령이
  바로 `--dump <그 프레임>` 이다.
* `--dump` 는 프레임을 PNG 로 떨군다 (`out/frame_00400.png`).
* `CS_TRACE=1` 을 켜면 HLE 호출이 인자·호출자와 함께 전부 찍힌다.

## 구성

| 경로 | 내용 |
|---|---|
| `cs_core/` | 런타임 전부. `mem` 게스트 주소공간, `elf` ELF32 로더, `heap` 게스트 할당자, `hle` import·JNI·`CletEntry` 구현, `gl` GLES1 캡처 + CPU 블리터, `fmt` printf, `files` fd/VFS |
| `cs_android/` | `libmain.so` (winit + softbuffer, GameActivity) |
| `cs_probe/` | 헤드리스 러너 |
| `vendor/arm32_cpu/` | ARM32 해석기 포크. ARMv5TE 명령(SMUL\<x\>\<y\>, SMLA/SMLAW/SMLAL, LDRD/STRD) 추가 + 디코드 LUT |
| `tools_re/` | 원본 바이너리 분석 스크립트와 심볼 덤프 |

## 이 저장소의 arm32_cpu 포크가 상류와 다른 점

상류(dlunch/arm32_cpu)는 ARMv4T(GBA) 대상이다. 이 게임은 ARMv5TE 라서 세 군데를
고쳐야 했고, 전부 `★ chronosSword fork` 주석이 붙어 있다:

1. **SMUL\<x\>\<y\> 계열** — 상류 디코드 표에 없어서 66곳의 `smulbb` 가
   `DataProc0` 로 잘못 해석되어 쓰레기값을 만들었다.
2. **LDRD/STRD** — `L==0, S==1` 조합을 상류는 "있을 수 없음"으로 단정한다.
   GCC 4.4 가 이 바이너리에 약 290개를 넣었다.
3. **`cond == 0b1111`** — ARMv5 에서는 조건이 아니라 무조건 명령 영역이다.
   BLX\<imm\> 만 통과시키고 나머지는 NOP 으로 넘긴다.
4. **`BLX <reg>`** — 상류 `BranchEx` 패턴이 bit 5 를 0 으로 고정해 `bx` 만 잡고
   `blx` 를 놓쳤다. 놓친 인코딩은 `DataProc1` 에 매치되어 no-op 으로 실행된다 —
   **간접 호출이 조용히 사라진다.** 이 바이너리에서는 `DM_DrawImg`/`DM_DrawImgEx`
   의 꼬리 호출(픽셀 루프)이 그것이라, 타이틀 일러스트·메뉴 배경·맵 타일이 전부
   안 그려지고 HUD 만 남았다.

추가로 **디코드 LUT**를 넣었다. 상류는 명령마다 최대 20번 mask-compare 를 한다.
`decode_lut_matches_linear` 테스트가 디코드 관련 비트 전 범위(2^28 × 2 워드)에
대해 LUT 와 원래 스캔이 같은 답을 내는지 확인한다.

```powershell
cargo test -p arm32_cpu --release
```

## 동작을 바꾸는 스위치

전부 환경변수와 **빌드 시점**(`option_env!`) 둘 다 읽는다 — 기기에는 환경이 없기
때문이다. `cs_core/build.rs` 가 이 이름들을 cargo 에 알려 주므로 빌드에 굽는 값을
바꿀 때 `cargo clean -p cs_core` 는 필요 없다(예전에는 필요했다).

붙을 서버는 노브가 아니라 빌드 스크립트 인자다 — `build_android.ps1 -Server host:port`
(기본 `off` = 리다이렉트 없음. 이 저장소에는 서버가 없다).

| 변수 | 기본 | 끄면 |
|---|---|---|
| `CS_FREE_CASH` | 켬 | 캐시 아이템이 다시 유료(도달 불가한 결제)로 돌아간다 |
| `CS_CASH_CONFIRM` | 끔 | 캐시 구입 확인창을 원본처럼 두 번 누른다 |
| `CS_DPAD_SCALE` / `CS_DPAD_DX` / `CS_DPAD_DY` | 125 / 4 / 4 | `100 / 4 / 2` 면 메뉴 D-pad 패치가 설치조차 안 된다 |
| `CS_PAD_MODE` | 음수 (세이브를 따름) | 0 유동 / 1 고정 / 2 고정+대체버튼을 매 로드마다 강제한다 — 환경설정의 「터치방식」을 덮어쓰므로 디버깅용 |
| `CS_PAD_MIGRATE` | 켬 | 환경설정이 만들 수 없는 값(2 이상)을 0 으로 되돌리는 것도 끈다 |
| `CS_OPT_DEBUG` | 끔 | 켜면 옵션 저장마다 `_menuOpt[9..0x19]` 와 호출자 LR 을 찍는다 (진단용) |
| `CS_SAVEXFER` | 켬 | 환경설정 맨 밑의 세이브 내보내기/불러오기/되돌리기 세 줄과 그 모달을 뺀다 (데이터 관리 줄은 `CS_OPTIONS` 가 켜진 한 계속 없다) |
| `CS_SLOW_FRAME_MS` | 80 | 이 시간을 넘는 프레임이 자기가 한 일(호스트 패치 카운터 차분)을 logcat 에 찍는다. 0 이면 끈다 |
| `CS_ALWAYS_SKIP` | 켬 | 컷신 SKIP 이 원본대로 나레이션·페이드에서 사라진다 |
| `CS_BAG_RIGHT` | 끔 | 1 이면 메뉴의 가방에서 → 가 가방을 연다 (원본은 퀘스트로 커서 이동) |
| `CS_CAST_FIX` | 켬 | 캐스팅 중단 뒷정리를 원본 그대로 둔다 |
| `CS_EQUIP_FIX` | 켬 | 로드 때 장비 슬롯을 가져가는 경쟁에서 원본대로 "가방 순서가 뒤인 쪽"이 이긴다 |
| `CS_SKIP_WATCHDOG` | 켬 | 렌더러가 안 도는 채로 걸린 컷신 스킵을 원본대로 영영 멈춰 둔다 |
| `CS_STRIKE_Y` | 4 | 취소선의 세로 위치 |
| `CS_NO_PATCH` / `CS_NO_FNT` / `CS_NO_SCREEN` / `CS_NO_ROT` / `CS_NO_DES` | 끔 | 호스트 구현 전부 / 폰트만 / `DM_DrawScreen565*` 만 / `DM_DrawRotate565*` 만 / DES 만 끈다 (A/B 용) |
| `CS_SCREEN_DEBUG` / `CS_ROT_DEBUG` | 끔 | 켜면 블리터 호출의 회전 코드·포맷·alpha·level 을 찍는다 |

## 알려진 제약

* **온라인 기능(편지·내 ID)은 기본으로 꺼져 있다.** 원본 서버는 없어졌다. 게스트의
  네트워크 코드는 멀쩡해서 호스트가 연결을 `-Server` 로 구운 주소로 돌릴 수 있지만
  (`cs_core/src/net.rs`), 이 저장소에는 서버가 없고 기본 빌드는 아무 데로도
  돌리지 않는다. 캐시 결제는 서버가 아니라 JNI(`AndroidService::requestMTX`)
  경로였고 지금은 호스트가 무료로 답한다.
* **환경설정 → 데이터 관리**는 더 이상 없다(2026-10-28). 기기를 바꿀 때는 그 자리의
  **세이브 내보내기 / 불러오기 / 되돌리기** 를 쓴다 — 폰의 파일 화면으로 `.cssave`
  한 파일(세이브 컨테이너 + 공용창고 + 비약)을 내보내고, 다른 폰에서 불러오면 앱이
  닫혔다가 다음 실행에 바뀐다(옛 세이브는 보관함 5세대, 되돌리기로 왕복). 자세히는
  `CLAUDE.md`.
* **진동 없음.**
* **프레임 예산을 넘는 프레임이 사실상 없다.** 블리터 네 개와 DES 까지 호스트로 간
  뒤(2026-08-12) 2600프레임 재생에서 33 ms 초과가 205개 → **1개**로 줄었고, 무패치
  인터프리터 대비 **210배**이면서 덤프 프레임이 픽셀 단위로 동일하다. 자세히는
  `CLAUDE.md` 의 "성능 4"·"성능 5".

## 이 호스트(Windows + gnu 툴체인) 특이사항

`build_android.ps1` 이 알아서 처리하지만, 직접 `cargo ndk` 를 부를 거라면:

* 안드로이드 크로스컴파일이어도 **호스트** build-dependency(`cc` → `windows-sys`,
  `getrandom`)가 빌드되고, 그 raw-dylib import 라이브러리 때문에 rustc 가
  `dlltool.exe` 를 찾는다. rustup 이 `self-contained` 에 넣어 둔 GNU dlltool 은
  같이 필요한 `as` 가 없어서 못 쓴다. `tools/dlltool_shim.rs` 를 빌드해
  PATH 앞에 두면 된다 (`target.<host>.rustflags` 는 build-dep 에 닿지 않는다).
* AGP 8.7 은 JDK 17+ 가 필요하다. PATH 의 첫 `java` 가 Java 8 이면
  "Dependency requires at least JVM runtime version 11" 로 죽는데, 이 메시지는
  `JAVA_HOME` 을 언급하지 않는다.
