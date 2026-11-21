# 크로노스 소드 — 원본 APK 분석과 64비트 이식 근거

2026-08-09. 분석 대상: `chronosSword/chronosSword32bit.apk`
(`co.kr.eamobile.chronosMian`, versionCode 10105) 와 `chronosSword.zip` 안의
`co.kr.eamobile.zip`(원본 기기 `/data/data` 덤프).

---

## 1. 결론부터

**원본 APK를 그대로 64비트 APK로 "변환"하는 것은 불가능하다.** 게임 본체는
`lib/armeabi/libchronosMian.so` 안의 **887 KB ARM32 기계어**이고 소스가 없다.
ARM32 기계어를 ARM64 기계어로 바꿔 주는 도구는 존재하지 않는다 (정적 리컴파일은
연구 수준이고 이 규모에서 신뢰할 수 없다).

**대신 가능한 것**: 프로세스 전체가 AArch64인 앱 안에서 원본 ARM32 코드를
**해석 실행**하고, 그 코드가 바깥세상과 만나는 **아주 좁은 경계면만** 네이티브로
구현하는 것. 이것이 이 저장소가 하는 일이고, 실제로 동작한다.

경계면이 좁다는 것이 이 판단의 핵심 근거다:

| 경계면 | 크기 |
|---|---|
| import 심볼 (libc + libm 3개 + GLES1 19개) | **79개** |
| JNI 진입점 (`Java_co_kr_eamobile_CletEntry_*`) | **14개** |
| 게스트가 호출하는 `JNIEnv` 슬롯 | **20개** |
| 게스트가 호출하는 Java 정적 메서드 (`CletEntry`) | **약 50개** |
| 그 외 (WIPI `MC_*` API 122개 포함) | **전부 .so 안에 있음** |

---

## 2. 원본 바이너리의 정체

```
ELF 32-bit LSB shared object, ARM, EABI5, dynamically linked,
with debug_info, not stripped
DT_NEEDED: libc.so libstdc++.so libm.so libGLESv1_CM.so libdl.so liblog.so
GCC: (GNU) 4.4.0        Tag_CPU_name = "5TE", Tag_CPU_arch = ARMv5TE
```

* **순수 ARM 모드**다. `.ARM.attributes` 에 `Tag_THUMB_ISA_use` 가 없고
  `Tag_VFP_arch` 도 없다 → Thumb 없음, VFP/NEON 없음, 소프트 부동소수점.
  해석기가 감당해야 할 명령어 집합이 그만큼 작다.
* **심볼이 살아 있다**: `.symtab` 에 함수 2734개, 객체 711개. `CHAR_*`,
  `MONSTER_*`, `HERO_*`, `ITEM_*`, `IGM_*` 같은 게임 로직 이름이 그대로 있다.
  이 포트가 디버깅 가능한 가장 큰 이유이며, 오류 메시지에 게스트 함수 이름을
  찍을 수 있는 근거다 (`Vm::describe`).
* **재배치가 교과서적**: `R_ARM_RELATIVE` 1990개, `R_ARM_GLOB_DAT` 1개,
  `R_ARM_JUMP_SLOT` 77개. 그 외 없음.
* DWARF 은 `midas` 이식 계층(EA Mobile 사내 포팅 레이어)에만 있다. 소스 경로가
  `/cygdrive/d/EAPK/kr_midas_console/mian_lib_dev/jni/midas/*.cpp` 로 남아 있다.

즉 이 게임은 **WIPI-C 클릿**을 `midas` 계층에 맞춰 컴파일한 것이다.
`startClet` / `pauseClet` / `resumeClet` / `destroyClet` / `handleCletEvent` /
`paintClet` 이 전부 있고, `MC_grp*` / `MC_knl*` / `MC_fs*` / `MC_mda*` /
`MC_net*` WIPI API 122개가 **.so 안에 구현되어** 있다.

## 3. 실행 구조 (원본 Java 쪽에서 알아낸 것)

`classes.dex` 를 뜯어 보면 `CletEntry.onSurfaceCreated` 가 시작 전체다:

```java
nativeInitKernel();
screenWidth/Height = 렌더뷰 크기 (없으면 디스플레이 크기);
adjustAutoScreen();                       // 400x240 을 화면에 맞추는 배율·여백
autoMarginY = nativeInitDisplay(sw, sh, 5,6,5,0,   // RGB565
                                400, 240, marginX, marginY, 0,
                                magnification, 0, 1, 0, 0);
nativeInitFileStorage(internalPath, externalPath);
nativeInitVFS(vfsFD, vfsStartOffset, vfsLength);
nativeInitAudioConfiguration(202400);
nativeStartApp();
// 이후 매 프레임: onDrawFrame -> nativeLoop()
```

* **논리 해상도는 400 x 240, RGB565**. `chronosMian` 액티비티 생성자가
  `SKTActivity(…, 400, 240, 0,0,0, 2.0f, 0, 1, 0, 0)` 을 호출한다.
* VFS(`assets/data.vfs.mp3`, 19 MB)는 Java 가 `AssetFileDescriptor` 로 열어
  **fd + offset + length** 를 네이티브에 넘긴다. 게스트가 직접 stdio 로 읽는다.
* 터치는 Java 가 `(raw - margin) / magnification` 으로 WIPI 좌표로 바꿔서
  `nativeTouchHandleEvent(19|20|21, x, y, pointerId)` 로 넘긴다.

## 4. 그래픽 — GL 을 거의 쓰지 않는다

이 게임은 스프라이트를 GL 로 그리지 않는다. `DM_Initialize` 가
`MC_grpGetScreenFrameBuffer(0)` 를 **한 번** 부르고, 그 뒤로는 게스트 ARM 코드가
그 프레임버퍼에 픽셀을 직접 쓴다. GL 트래픽은 이것뿐이다:

```
MC_grpFlushLcd(...)  ->  Texture2D::bindTextureFromDirect(...)  ->  glTexSubImage2D
CletEntry.onDrawFrame -> nativeLoop -> updateScreen -> Texture2D::update
                                       -> glVertexPointer/glTexCoordPointer/glDrawArrays
```

`glDrawArrays` 는 항상 `GL_TRIANGLE_STRIP`, 정점 4개, 축에 정렬된 사각형이다.
검은 레터박스 막대는 같은 텍스처에 `glColor4f(0,0,0,1)` 을 곱해서 그린다.

그래서 `cs_core/src/gl.rs` 는 OpenGL 구현이 아니라 **이 호출들을 지켜보다가
사각형 하나를 CPU 로 확대 복사하는 상태 기계**다. 헤드리스 프로브와 안드로이드
앱이 완전히 같은 경로를 쓰게 되고, 비용은 프레임당 400x240 스케일 복사 한 번이다.

## 5. VFS 포맷

`MIDASKernelManager::getAssetSizeFromVFS` 를 디스어셈해서 알아낸 형식:

```
[u32 nameHash][u32 size][size bytes] [u32 nameHash][u32 size][...] ...
```

이름 → id 는 해시가 **아니다**. `MidasAssetManager` 가 `Hashtable` 두 개로
"처음 본 순서대로 1부터" 번호를 붙이고, `loadAssetFromVFS` 가 그 번호로 이름을
되찾아 해시를 계산한다. 이 왕복을 그대로 재현하지 않으면 폰트 리소스부터
로드에 실패한다 (실제로 처음 부팅 때 `MX_fntSetProperty` 에서 널 참조로 죽었다).

## 6. 지금 상태와 성능

부팅 → 인트로 → SMS 동의 팝업 → 등급 고지 → 인게임 HUD 까지 확인했다.
한글 폰트, 알파 합성, 터치 UI 전부 원본대로 렌더링된다.

이 PC(데스크톱) 기준 **약 94 M guest inst/s**, 기기(S21)는 **약 46 M**.

### 처음에는 게스트 명령어의 3분의 2가 함수 하나에 있었다

```
  67.8%  _Z26internalGrpCopyFrameBuffer...   ← WIPI 프레임버퍼 블리터
  16.5%  DM_DrawRotate565Ex
   6.9%  MC_grpFillRect
```

그 뒤로 블리터·폰트·박스블러·나눗셈을 차례로 호스트로 옮겼고, 마지막에
`DM_DrawScreen565`/`565Ex` 와 거절하던 전치 회전 코드까지 옮겼다(2026-08-12).

### 지금 (같은 2600프레임 결정적 재생, 부팅→메뉴→클래스→인트로 전투, 400x240)

| | 게스트 명령 | 프레임 평균 | p99 | 최악 | 33 ms 초과 |
|---|---|---|---|---|---|
| 패치 없음 (`CS_NO_PATCH=1`) | 150.73억 | 126.0 ms | 323.2 ms | 972.6 ms | 사실상 전부 |
| 성능 패치 전부 | **7272만** | **0.6 ms** | **2.2 ms** | 47.6 ms | 2/2600 |

**207배**, 그리고 덤프 프레임 12개가 무패치 인터프리터와 **픽셀 단위 동일**하다
(의도적으로 화면을 바꾸는 `CS_ALWAYS_SKIP` 등은 끈 상태로 비교 — 켜면 SKIP
버튼 하나가 늘어나는 것이 유일한 차이다).

프로파일은 이제 평평하다. 어떤 함수도 10%를 넘지 않는다.
**그릴 것이 병목인 시대는 끝났다.**

### 평균이 아니라 분포로 봐야 한다

"보스전 끝나고 렉이 걸린다"는 증상은 평균으로는 안 보인다. 패치 이전 상태에서도
평균은 10.6 ms(= 94 fps)였지만 p99 가 84.6 ms, 최악이 310 ms 였다. 그래서
`cs_probe` 가 매 실행 끝에 프레임 비용 분포와 **가장 비싼 프레임 번호**를 찍는다
(`report_frame_cost`). 번호를 알면 다음 명령이 바로 `--dump <그 프레임>` 이다.

## 7. 남은 것

* **사운드**: `CletEntry` 의 오디오 콜백은 전부 "클립 없음"을 돌려준다. 게스트는
  이 상태를 스스로 감당하도록 짜여 있어서(미디어 볼륨 0인 기기와 같다) 조용히
  진행되지만, 아직 소리가 안 난다.
* **네트워크**: 해결됨(2026-08-11). 사설 서버에 실제로 붙는다 — 아래 별도 절.
* **진동**: 미연결.
* 게스트가 인트로 중에 `_midas_funcFntWanToJohab` 에서
  `ERROR : 0x3F,0x1E` 를 매 프레임 찍는다. 완성형 한글이 아닌 2바이트열(`'?'`
  다음 제어문자)을 변환하려다 나는 것으로, 원본에도 있던 잡음이고 화면에는
  `?` 가 정상적으로 그려진다.

## 8. 재현용 도구

`tools_re/` 에 분석에 쓴 것들이 들어 있다 (capstone + pyelftools 필요).

```
python tools_re/csdis.py payload/libchronosMian.so MC_grpFlushLcd HF_Initialize
python tools_re/xref.py  payload/libchronosMian.so stat qsort vsnprintf
```

`guest_functions.txt` / `guest_objects.txt` 는 원본 `.symtab` 을 주소순으로
덤프해 둔 것이다.

## DM_DrawRotate565Ex — 완전 해부 (2026-08-10: 호스트 이식 완료)

프레임의 94%를 쓰는 함수. `0x47c2c`, 크기 `0xaa0`. 인자 19개 —
`push {r4..fp,lr}`(36) + `sub sp,#0xac` 라서 **스택 인자 N 은 `sp+0xd0+4N`**.

```
r0 dst(u16*)  r1 dstX  r2 dstY  r3 w
+0xd0 h      +0xd4 dstW  +0xd8 dstH  +0xdc img*
+0xe0 sx     +0xe4 sy    +0xe8 sw    +0xec sh
+0xf0 rot(0..3,4/6)      +0xf4 angle(0/90/180/270)
+0xf8 scaleX%            +0xfc scaleY%
+0x100 alphaLevel(0..0x10)  +0x104 alpha(0..255)  +0x108 anchor
```

관측된 호출(타이틀): `103x121, rot=0, angle=0, scale=100/100, alphaLevel=0,
alpha=255, anchor=9`.

### 좌표 계산

```
scaledW = w*scaleX/100 (>=1)      scaledH = h*scaleY/100 (>=1)
xstep   = (w<<10)/scaledW + 1     ystep   = (h<<10)/scaledH + 1   ← +1 주의
DM_CalcAnchorGbm(&dx,&dy, scaledW,scaledH, rot, angle, anchor)
```
`DM_CalcAnchorGbm`(0x46a10)은 anchor 비트만 본다 — `0x10`이면 `dx -= sw/2`,
그 안에서 `2`면 `dy -= sh/2`, 아니고 `4`면 `dy -= sh`. angle 이 90/270 이면
가로세로를 한 번 더 맞바꾼다. anchor=9 → `0x10` 아님, `4`도 아님 → **보정 없음**.

### 소스 픽셀

`fmt = *(u8*)img->[0x20]` 이 **0xb 이면 4bpp, 0xc 이면 8bpp** 팔레트다.
(일반 경로는 `[sp+0x70]`/`[sp+0x78]`을 초기화하지 않는다 — 즉 실제 호출은
반드시 이 둘 중 하나로 들어온다. 이게 오래 막혔던 부분이다.)
`img->[0x1c] = img->[0x10]` 이 팔레트(u16 RGB565), stride 는
`(img->[0x14]+4)*2` 를 기준으로 `0x48620`(4bpp) / `0x48654`(8bpp) 블록이
`[sp+0x70]=stride`, `[sp+0x78]=base` 로 넣고 본류로 돌아온다.

### 픽셀 루프 (rot=0)

```
for y in 0..scaledH:
    srcY   = yacc >> 10
    rowoff = srcY*stride + base
    for x in 0..scaledW:
        srcX = xacc >> 10
        idx  = 8bpp ? pix[rowoff+srcX]
                    : (srcX&1 ? pix[rowoff+srcX/2]&0xf : pix[rowoff+srcX/2]>>4)
        c = palette[idx]
        if c == transparent: continue          # 전역 B
        if x < clip.x1 || y < clip.y1 || x >= clip.x2 || y >= clip.y2: continue
        write
        xacc += xstep
    yacc += ystep
```

### 블렌드 (2026-08-10 정정: 전 경로 디스어셈블 완료, 호스트 이식 완료)

~~`alphaLevel == 0x10` 이면 그냥 쓴다~~ — **틀렸다**. 0x481a0 을 읽으면
`level == 0x10` 은 **그레이스케일 모드**다: `gray = (6·r5 + 4·g5 + 2·b5) >> 4`,
세 채널 모두 gray (녹색은 gray*2 로 6비트). 그 외 level 은 **32x32 룩업 테이블**:

```
out_ch = table[(alphaLevel*32 + src_ch)*32 + dst_ch]      # ch = r,g,b 각 5비트
```
녹색도 `(c&0x7e0)>>6` 로 **5비트**만 쓰고 결과를 2배 해 6비트로 되돌린다.

그 **다음에** `alpha <= 0xfe` 면 위 결과를 dst 와 한 번 더 가중 혼합한다(0x47ff0).
`a5 = (alpha&0xf8)>>3`, `a6 = (alpha&0xf8)>>2` 로:

```
r_out = (r·(a5+1) + dst_r5·(32-(a5+1))) >> 5
g_out = (g6·(a6+1) + dst_g6·(64-(a6+1))) >> 6     # dst_g6 = dst_g5*2 (LSB 유실)
b_out = (b·a5     + dst_b5·(32-a5))     >> 5      # 파랑만 +1 없음 — 출하된 비대칭
```

(rot, angle) 은 내부 코드 하나로 접힌다: 0..3 = 무변환/좌우/상하/180 (행 우선),
4..8 = 90°/270° 전치 (열 우선). `DM_DrawRotate565`(0x486cc)는 같은 구조에서
블렌드 파이프라인만 없는 불투명 버전이고, 리터럴 풀 위치만 다르다
(GOT 리터럴: Ex 는 entry+0xa8c, 비-Ex 는 entry+0x638).

테이블 주소는 PIC GOT 경유지만 전부 PC 상대라 호스트가 정적으로 계산할 수 있다:

```
GOT   = (0x47c48+8) + u32[0x47c38+8+0xa78]
table = u32[GOT + u32[0x47dbc+8+0x904]]
```

`blend565`(산술 블렌드)는 **이 함수에는 쓰면 안 된다** — 여기는 테이블 방식이다.

`patch.rs::host_rotate_blit` 이 위 전부(코드 0..3, 배율, 앵커, 그레이, 테이블,
가중 혼합)를 구현했고, 전치 경로(4..8)와 비팔레트 포맷만 게스트로 넘긴다.
`CS_NO_PATCH=1` A/B 로 두 해상도 구성, 33개 프레임 픽셀 단위 동일 확인.
투명색 비교는 **32비트**다(`cmp r3, sl` — 팔레트 u16 vs 전역 u32 워드): 상위
비트가 있으면 아무것도 투명하지 않다. u16 으로 자르면 틀린다.

## 폰트 스택 — 완전 해부 (2026-08-10: 호스트 이식 완료)

회전 블리터 이후 타이틀 프레임의 88%가 폰트 다섯 함수에 있었다
(`_midas_funcFntDrawString` 59.7% + `FntDrawPixelPreRGB` 15.6% +
`MC_grpGetRGBFromPixel` 6.9% + `FntDrawPixel` 6.0%). 느린 이유는 구조적이다:
**루프 카운터·커서·스크래치 전부가 GOT 간접 전역**이라(`_midas_r0`,
`_midas_drawx`, `_midas_rgb_*`, …) 픽셀 하나에 리터럴 로드 + GOT 로드 +
포인터 로드가 반복되고, 켜진 픽셀마다 함수를 3개씩 부른다.
`tools_re/csdis_got.py` 가 이 GOT 트래픽을 심볼 이름으로 풀어 준다.

알아낸 구조 (전부 `cs_core/src/font.rs` 에 이식):

* 폰트 객체 `F = *_mxfnt_prt`: `F[4]/F[8]` 한글 셀 폭·높이, `F[0xc]/F[0x10]`
  ASCII 전진폭·행수, `F[0x14]` 자간, `F[0x18]` 공백폭, `F[0x1c]` y 오프셋,
  `F[0x24]` 알파, `F[0x28]` 모드(1=이중타, 2=외곽+본체, 3=외곽만, ≤0=본체만),
  `F[0x30]` 한글 성분당 바이트(0x20), `F[0x34]`=8 이면 컴팩트 자모 테이블,
  `F[0x3c]` ASCII 글리프 스트라이드, `F[0x40]/F[0x44]` 본체/외곽 색.
* `_midas_internalFont[0..4]` = 초성/중성/종성/ASCII 비트맵 베이스
  (`FntInvalidate` 가 리소스에서 계산).
* 한글: `WanToJohab`(완성형→조합형; A4 행 낱자·A1A4/A1A5 특례·0xda 파손행,
  범위 밖은 printk 후 `?` — 인트로의 `ERROR : 0x3F,0x1E` 잡음이 이 경로) →
  `Get3Syllable`(CJJ 테이블) → 변형 인덱스(`ChoInfo`/`JungInfo`/`JongInfo` =
  `F[0x20]` 블록) → 성분 비트맵을 32바이트 `_midas_abBuf` 에 OR 합성 →
  행당 2바이트(짝수 바이트가 **오른쪽** 절반, x+8 에 그림) 렌더.
* 블렌드: `_midas_internalFntOpacity[(A*16 + src>>4)*16 + dst>>4] << 4`
  (채널당, A = alpha>>4). 소스 채널은 `_midas_rgb_{r,g,b}0` **누산기 전역**에
  살고 결과가 다시 그 전역에 쓰인다 — LUT 결과(<<4)가 0xff0 까지 가면
  다음 인덱스가 4 KB 테이블을 **넘어** 뒤 메모리를 읽는다. 호스트는 LUT 를
  8 KB 깊이로 복사해서 그 초과 읽기까지 재현한다.

검증: `CS_NO_PATCH=1` A/B, 400x240/533x240 두 구성 × 부팅→슬롯→인게임 대사
2000프레임, 34개 덤프 전부 픽셀 단위 동일. 주의 — **storage 디렉터리를 매
실행 새로 줘야** 재생이 결정적이다(세이브 파일이 실행 간에 이월된다).

같은 방법으로 인게임 프레임 34%였던 `DM_FillRectAlphaRGB` 도 이식했다
(565 직접 경로만; `_lcdG[0x8a] != 1` 의 함수포인터 경로는 거절).

## DM_DrawScreen565 / 565Ex — 완전 해부 (2026-08-12: 호스트 이식 완료)

이 둘이 마지막이자 가장 컸다. 타이틀 프레임의 **99.4%**, 이펙트가 심한 인트로
전투의 **73.6%**.

### 어디에서 불리는가 — `bl` 이 하나도 없다

전 `.text` 를 스캔해도 두 함수로 가는 `bl` 이 **0개**다. `.got` 에만 주소가
있고, 진입은 픽셀 포맷으로 고른 함수포인터를 통한 간접 호출 세 곳뿐이다:

| 호출 지점 | 형태 | 목적지 |
|---|---|---|
| `DM_DrawImg+0x198` (0x47314) | `blx ip` | `DM_DrawScreen565` |
| `DM_DrawImgEx+0x1b0` (0x46c6c) | `blx ip` | `DM_DrawScreen565Ex` |
| `DM_OffDrawImg+0x18c` (0x48ec8) | `mov lr,pc / ldr pc,[ip]` | `DM_DrawScreen565` (오프스크린 GBM) |

세 번째를 놓치면 안 된다 — 진입점에 패치를 걸면 그것도 서브하게 되므로, 그
호출자에 대해서도 맞아야 한다. (이 바이너리에서 `blx <reg>` 가 실행되는 곳이
딱 두 군데라는 사실이 예전에 `BLX <reg>` 디코드 버그를 잡은 단서였다 —
CLAUDE.md 의 vendor 포크 절.)

### 왜 이식이 작았나 — 호출자가 클리핑을 끝낸다

세 호출자 전부 같은 구조다: 클립 사각형과 교차, 비면 조기 반환, 넘치는 만큼을
`dx/dy/w/h/sx/sy` 에 되접기, 그 다음 디스패치. `DM_OffDrawImg` 는 먼저
`DM_SetCliptoGBM(0, 0, gbm[0x50], gbm[0x4c])` 로 클립을 오프스크린 전체로
세우고, 끝나면 원래대로 돌린다. 전치 코드(4/6/7/8)는 목적지 사각형이 뒤집히므로
`DM_DrawImg` 가 **별도 클립 블록**(0x47350)에서 `x+h` 를 좌우에, `y+w` 를
상하에 대고 검사한다.

그 결과 두 블리터는 **클립 전역을 아예 읽지 않는다**. PC 상대 리터럴로 읽는
것은 `trans` 하나(Ex 는 블렌드 테이블 하나 더)뿐이다:

| | GOT 잡기 | trans | 블렌드 테이블 |
|---|---|---|---|
| `DM_DrawScreen565` (0x3eb04) | `진입+0x24 + [진입+0xf48]` | `[진입+0xf4c]` → GOT+0x6cc | — |
| `DM_DrawScreen565Ex` (0x401e8) | `진입+0x1c + [진입+0xefc]` | `[진입+0xf00]` → GOT+0x6cc | `[진입+0xf04]` → GOT+0x894 |

두 슬롯(+0x6cc, +0x894)은 `DM_DrawRotate565*` 가 쓰는 슬롯과 **같다**. GOT 를
잡는 `add rX,pc,rX` 가 한쪽은 진입+0x24, 한쪽은 **진입+0x1c** 인 것만 다르다 —
리터럴 표를 공유하면 여기서 틀린다.

### 서명

```c
DM_DrawScreen565  (u16 *dst, int dx, int dy, int w,
                   /*arg0*/ int h, /*1*/ int dstW, /*2*/ int dstH, /*3*/ Img *img,
                   /*4*/ int sx, /*5*/ int sy, /*6*/ int imgW, /*7*/ int imgH,
                   /*8*/ int code);
DM_DrawScreen565Ex(... 위와 같고, /*9*/ int alpha, /*10*/ int level);
```

* `dst` 는 프레임버퍼 구조체가 아니라 **생 `u16*`** 다. `dstW` 가 폭이면서
  동시에 행 스트라이드다 — 별도 pitch 가 없다.
* `arg[2] dstH`, `arg[6] imgW`, `arg[7] imgH` 는 **어느 코드 경로도 읽지 않는다**
  (`[sp+0x70]`/`[sp+0x80]`/`[sp+0x84]` 로 grep — 0건). 호출자가 넘기기만 한다.
  소스 기하는 전부 `img[0]` 에서 나온다.
* 본문 기준 스택 인자 위치가 다르다: `565` 는 `push 8 + sub 0x48` 이라
  `arg[N] = [sp+0x68+4N]`, `Ex` 는 `sub 0x78` 이라 `[sp+0x98+4N]`.
* **`Ex` 의 alpha/level 순서가 `DM_DrawRotate565Ex` 와 뒤집혀 있다** —
  여기는 `arg[9]=alpha, arg[10]=level`, 저기는 `arg[12]=level, arg[13]=alpha`.
  `DM_DrawImgEx` 쪽 데이터 흐름으로 확인: 0x46c28 `ldr r3,[sp,#0x88]` →
  0x46c38 `str r3,[sp,#0x24]`(나가는 arg[9]), 0x46c40 `ldr r3,[sp,#0x8c]` →
  0x46c4c `str r3,[sp,#0x28]`(arg[10]).

### 소스 주소 — 회전 코드 아홉 개가 전부 같다

```c
head = (img[0x14] + 4) * 2 + 1;
img[0x1c] = img[0x10];            // 0x3eb50, 포맷 검사 전에 일어나는 부수효과
fmt = *(u8*)img[0x20];            // 0xb = 4bpp, 0xc = 8bpp, 그 외 즉시 반환
4bpp: stride = (img[0] + (img[0] & 1)) >> 1;  base = head + (sx >> 1) + stride*sy;
8bpp: stride =  img[0];                       base = head +  sx       + stride*sy;

// 출력 (i, j), i = 0..w-1, j = 0..h-1
8bpp: idx = pix[base + j*stride + i];
4bpp: k = i + (sx & 1);
      b = pix[base + j*stride + (k >> 1)];
      idx = (k & 1) ? (b & 0xf) : (b >> 4);     // 짝수 절대열 = 상위 니블
s = pal16[idx];  if ((u32)s != trans) 쓴다;      // ldrh 라 상위 16비트가 선 trans 는 아무것도 안 맞음
```

게스트는 이걸 "홀수 헤드 → 두 픽셀씩 짝 루프 → 홀수 테일 → 행 전진" 네 조각으로
풀어 쓰는데, 짝 루프가 목적지 포인터 **두 개**를 한 픽셀 간격으로 들고 각각 두
픽셀씩 전진한다. 위 식은 그것과 동치다 — 짝/홀 어느 `sx` 에서도, 음수 `sx` 에서도
(`sx>>1` 은 ASR, `sx&1` 은 생 AND).

### 목적지 — 아홉 코드가 아핀 식 하나로 접힌다

열여덟 블록이 소스를 전부 같게 걷고 차이는 어디에 쓰는지뿐이므로
`D(i,j) = d0 + j·DR + i·DI` 로 끝난다. 표는 CLAUDE.md 의 성능 4 절에 있다.
`D` 가 아핀이라 극값이 네 꼭짓점에 있고, 그래서 전치 코드까지도 **목적지가 연속
구간 하나**다 — 블록으로 읽고 고쳐서 한 번에 되쓴다.

`code` 는 `DM_DrawImg`/`DM_DrawImgEx` 가 `(rot, angle)` 에서 만든다. 순수 동등
비교 체인(0x3edd8 4bpp / 0x3efac 8bpp)이라 **0..8 밖은 아무것도 안 그리고 반환**
한다.

**코드 5 함정.** `DM_DrawImg+0x3b4` 가 `rot==0, angle==0xb4` 에 5를 내고, 그
블록은 전치가 아니라 **행 우선 180°**, 코드 3과 비트 단위로 같다. 그런데
`DM_DrawRotate565Ex`/`565` 에는 `mov r4,#5` 가 아예 없고 같은 180°를 3으로
번호붙인다(모든 `r4` 대입 지점을 전수 조사해 확인). **두 함수의 코드 표는 별개다.**

### Ex 의 블렌드는 네 본체가 전부 같은 산술

`DM_DrawRotate565Ex` 의 행 우선/전치 본체와 `DM_DrawScreen565Ex` 의 두 본체가
레지스터 배치만 다른 같은 코드다. 그래서 호스트는 `Blend565` 하나로 공유하고,
두 번째 이식이 첫 번째 이식의 A/B 증거를 물려받는다.

```c
sr = s>>11;  sg5 = (s & 0x7e0) >> 6;  sb = s & 0x1f;      // 초록을 5비트로 뽑는다
level == 0x10:  gray = (6*sr + 4*sg5 + 2*sb) >> 4;  pr = gray; pg6 = gray*2; pb = gray;
그 외:          pr = T[sr][dr];  pg6 = T[sg5][dg5] * 2;  pb = T[sb][db];
                  T[a][b] = tbl[level*1024 + a*32 + b]
alpha <= 0xfe:  pr  = (pr*a5p1 + dr*ia5p1)      >> 5;
                pg6 = (pg6*a6p1 + (dg5*2)*ia6p1) >> 6;
                pb  = (pb*a5    + db*ia5)        >> 5;     // 파랑만 증가 안 한 가중치
out = (pr << 11) | ((pg6 << 5) & 0x7e0) | (pb & 0x1f);     // 빨강은 마스크 없음
af = alpha & 0xf8;  a5 = af>>3;  a5p1 = a5+1;  a6p1 = (af>>2)+1;
ia5 = 32-a5;  ia5p1 = 32-a5p1;  ia6p1 = 64-a6p1;
```

### 전치 회전 코드 4/6/7/8 (`DM_DrawRotate565*`)

`code > 3` 이면 거절하던 것을 이식했다. 내부 루프가 목적지 **행 하나**를 통째로
넘어가고(`add ip,ip,r3`, `r3 = dstW*2`, 0x48504) 외부 루프가 한 픽셀 오른쪽으로
간다(`add r2,r2,#2`, 0x48560):

```c
dstAddr(i, j) = dst + 2*((dy + i)*dstW + (dx + j))    i in [0, sw), j in [0, sh)
col = src_ix(i);   row = src_iy(j);
```

* 목적지 사각형이 **`sh` 폭 × `sw` 높이**로 뒤집힌다. 즉 `scaleX` 가 회전된
  스프라이트의 **세로** 크기를 정한다(퍼센트가 소스 축을 따라가므로).
* 그런데 `xstep`/`ystep` 은 같이 뒤집히지 않는다 — 여전히 소스 자기 폭·높이의
  스텝이고, 공통 프롤로그에서 한 번 계산된 그 값이다. 화면 축마다 **반대쪽**
  소스 축이 붙는 것뿐이다.
* 그래서 **`row_off` 를 행마다 미리 계산하면 안 된다.** 전치에서는 소스 행이
  내부(x) 인덱스에 따라 변하고 소스 열이 외부(y) 인덱스로 고정된다. 행 우선
  코드에서 하던 대로 호이스팅하면 각 행의 첫 픽셀만 맞는다.
* `hflip = {1,3,6,7}`, `vflip = {2,3,4,7}`.
* 클립은 화면 축에 붙으므로 검사는 그대로고 **범위만** 교환된다:
  `x ∈ [cl, min(dx+sh, cr))`, `y ∈ [ct, min(dy+sw, cb))`.
* 앵커는 손댈 것이 없다. 전치 본체도 `DM_CalcAnchorGbm(&dx,&dy,sw,sh,angle,anchor)`
  를 **명령 단위로 같게** 부른다. 전치 코드는 구성상 angle 이 항상 0x5a/0x10e 라
  스왑 블록이 늘 돌고, 그것이 중앙 정렬을 전치된 크기에 맞춰 주는 보정이다.
  다만 스왑 블록은 `anchor&0x10`/`anchor&2` 로만 걸려 있어서 우측/하단 앵커
  (`0x20`/`4`)는 **전치 안 된 크기**를 빼서 `sw-sh` 만큼 틀린다 — 원본 버그이고
  그대로 재현한다. 실제로 쓰이는 18곳은 전부 `anchor = 9` 라 무영향.
* 10.10 스텝의 `+1` 때문에 마지막 소스 인덱스가 소스 끝을 **넘을 수 있다**
  (`w=3, sw=200` → `xstep=16`, `i=199` → `col=3`, `w-1=2`). 게스트가 행 뒤
  1바이트를 읽으므로 호스트도 그 바이트를 span 에 넣어야 한다(클램프하면 안 됨).
  행 우선 코드에도 원래 있던 성질이다.

### 검증

`CS_NO_SCREEN=1`(스크린 블리터만) / `CS_NO_ROT=1`(회전 블리터만) /
`CS_NO_PATCH=1`(전부) 로 A/B. 400x240·533x240 두 구성 × 2600프레임 ×
덤프 24프레임 전부 **픽셀 단위 동일**. 무패치 인터프리터와의 최종 대조도
12/12 동일하다(단, 화면을 의도적으로 바꾸는 `CS_ALWAYS_SKIP` 등은 끄고 비교 —
켜면 SKIP 버튼 하나가 유일한 차이).

주의 — **storage 디렉터리를 매 실행 새로 줘야** 재생이 결정적이다.

## 세이브·몬스터 로드의 DES (2026-08-12: 호스트 이식 완료)

렌더링을 다 옮긴 뒤 남은 유일한 33 ms 초과 프레임(202 ms)이 이것이었다.

`MX_desDecrypt` **한 번**(1,832바이트)이 **약 640만 게스트 명령** = 기기 139 ms.
바이트당 3,500 명령인 이유는 이 DES 가 **비트 배열**(1비트를 1바이트에) 위에서
돌기 때문이다 — `startDes`(0x2e984)가 0x100/0x80 바이트 버퍼를 여섯 번 memset
하고, `desRound`(0x2b8f4)가 네 번 더 한다.

호출 사슬과 빈도:

```
MONSTER_addBody → MONSTER_dataLoad → MD5_LoadData → MX_desDecrypt   ← 캐시 없음
GameFrame_autoSave → GameFrame_save_hero_data → SaveGameBase → MX_desEncrypt
                   → GameFrame_saveGame       → SaveGameBase → MX_desEncrypt
LoadGameBase ← GameFrame_loadGame / load_hero_data / HERO_save_data /
               MainMenuFrame_Initialize
onStartApp → MX_desInit (부팅 때 한 번)
MX_desEncrypt / MX_desDecrypt → startDes → desRound (두 번)
```

`MONSTER_dataLoad`(0xd0bd4)는 파일명을 `MC_knlSprintk` 로 만들어 읽고 복호화하고
`MC_knlFree` 한다 — **캐시가 없어서 몬스터가 생길 때마다 다시 복호화한다.**

오토세이브 쪽은 크기가 바이너리에 박혀 있다. `SaveGameBase(name, buf, len)` 은
`MC_knlCalloc(len)` 사본에 `MX_desEncrypt(copy, len)` 를 걸고 `MX_sdSaveData` 로
쓴 뒤 사본과 원본을 둘 다 해제한다(0x5f89c, 0x5c바이트) — 즉 호출자의 버퍼는
평문으로 남는다. 두 호출자의 `len`:

| 호출자 | len |
|---|---|
| `GameFrame_save_hero_data+0x2c8` | `mov r2,#0x1800; add r2,r2,#7` = **6,151** |
| `GameFrame_saveGame+0x318` | `mov r2,#0xc00` = **3,072** |

`GameFrame_autoSave` 는 이 둘을 연달아 부른다 = **9,223바이트**. 3,493 명령/바이트를
곱하면 약 **3,220만 명령 = 기기에서 약 0.7초**. 한 프레임 안에서 일어나므로 그대로
0.7초 정지다. 이것이 "보스전이 끝난 이후"와 "몬스터가 많을 때"가 같은 증상인 이유다.

측정 근거와 그 한계: 3,493 명령/바이트는 프로파일에서 `desRound` 가 1,832바이트
복호화 **한 번**에 쓴 640만 명령이다(그 런에 DES 호출이 하나뿐임을 `CS_WATCH` 로
확인). `startDes`/`MX_desDecrypt` 자체는 상위 20위에 안 들 만큼 작다. 반면 위
0.7초는 **측정값 × 박힌 크기**이고 실기기 오토세이브를 직접 계측한 것은 아니다 —
프로브 재생만으로는 `SaveGameBase` 에 도달하지 않는다. (`out/storage/kickass`
1,060바이트는 DES 세이브가 **아니다**: 엔트로피 0.10 bits/byte 의 평문 옵션·동의
레코드다.)

### 어디를 후킹하는가 — `startDes`, 래퍼가 아니라

`cs_core/src/des.rs` 는 `startDes(mode, blocks, buf)` 를 대신한다. 방향에 따라
달라지는 상태가 여기에는 없다는 것이 이유다:

```
MX_desEncrypt 0x2ed50..0x2ed84 : if (KEY4ENCRYPT == 0) { KEY4REAL 16블록 역순 } ; KEY4ENCRYPT = 1
MX_desDecrypt 0x2ecb0..0x2ece4 : if (KEY4ENCRYPT != 0) { 같은 역순 }             ; KEY4ENCRYPT = 0
                                 j = 0..7 : 블록 j <-> 블록 15-j  (rsb r0,r6,#0x2d0)
```

즉 0x180 은 영역 크기가 아니라 **바깥 루프 한계**다(한 번에 한 쌍을 바꾸므로
0x300 의 절반). 이 뒤집기는 `startDes` **호출 전**에 끝나고 `desRound` 는 언제나
라운드 0..15 를 정방향으로 읽는다. 그래서 공용 드라이버를 후킹하면 게스트가 방금
세운 순서를 그대로 물려받고 **플래그를 모델링할 필요가 없다**. `KEY4ENCRYPT` 의
`.data` 초기값은 0(이 아니)라 0xff 이고 `MX_desInit`(0x30284)이 1로 세운다.

### 세 가지 함정 — 교과서 DES 를 쓰면 세이브가 깨진다

| # | 무엇 | 결과 |
|---|---|---|
| 1 | `MAT_SBOX` 0x114952 = S1 row3 col10 이 `2`, FIPS 는 `3` | 8바이트 블록 넷 중 하나쯤이 달라진다 |
| 2 | 최종 스왑 없음 — `IP⁻¹(L16‖R16)` | 전부 달라진다 |
| 3 | 블록 수 = `len/8` 절삭 | 꼬리 `len%8` 바이트가 평문으로 남는다 |

1번은 대체 S-box 가 아니라 **오타**다: 그 행이 0..15 의 순열이 아니다(2가 둘, 3이
없음), 그리고 32개 행 중 그 검사에 실패하는 유일한 행이다. `MX_desEncrypt` 의
블록 수는 `(len+7)>>3` 이 **아니다** — `add r3,r8,#7 / cmp r8,#0 / movlt r8,r3 /
asr r1,r8,#3` 은 부호 있는 8로 나누기 관용구이고 `+7` 은 음수 보정일 뿐이다.

그래서 테이블은 전부 **심볼 이름으로 게스트 메모리에서 읽는다**:
`MAT_IP`(0x114848, 64) · `MAT_IP_1`(0x114888, 64) · `MAT_EXP`(0x1148c8, 48) ·
`MAT_PERMU`(0x1148f8, 32) · `MAT_SBOX`(0x114918, 512) · `KEY4REAL`(0x123cb8, 768).
순열 테이블은 모두 **1-based** 다. 비트 순서는 어디서나 **MSB first**
(`char2binary` 0x2bb8c: `dst[i*8 + (7-b)] = (src[i]>>b)&1`; `binary2char` 0x2bbd8
은 **0 아닌 워드를 전부 1로** 본다).

알고리즘 본체:

```c
// startDes, 블록마다
X[i] = bits[MAT_IP[i]-1]                                   i<64
encrypt: A = X[0..32],  B = X[32..64]                      // 0x2eb14
decrypt: B = X[0..32],  A = X[32..64]                      // 0x2ec38, 교차
desRound(A, B)                                             // 16라운드 전부
encrypt: swap32bit(A, B)                                   // 0x2eb48, decrypt 는 건너뜀
out = B ‖ A                                                // 0x2eb50, 항상
y[i] = out[MAT_IP_1[i]-1]                                  i<64
// desRound, 라운드마다
Ltmp = R
exp[i] = R[MAT_EXP[i]-1] ^ KEY4REAL[round*48 + i]          i<48
n = 0..8: x = exp[6n..6n+6]
          v = MAT_SBOX[64n + 16*(2*x[0]+x[5]) + 8*x[1]+4*x[2]+2*x[3]+x[4]]
          sb[4n..4n+4] = v 의 니블, MSB first
pm[i] = sb[MAT_PERMU[i]-1]                                 i<32
R[i] = pm[i] ^ L[i];  L[i] = Ltmp[i]                       // R 을 먼저 쓴다
```

### 그리고 게스트 자신에게 물어본 다음에야 쓴다

`Vm::boot` 끝에서 `des_selftest()` 가 **양방향 모두** 게스트의 `startDes` 를
`Vm::call` 로 돌려 128바이트를 왕복시키고 호스트 결과와 비교한다. 다르면 그 방향은
프로세스 내내 인터프리터에 맡기고 `log::error!` 를 찍는다. DES 에는 데이터
의존 분기가 없으므로 16블록 일치는 256번의 S-box 조회와 순열망 전체를 거친 것이다.
비용은 부팅 때 한 번 **100만 명령**(기기 22 ms).

8블록이 아니라 16블록인 이유는 **함정 하나만 확률적으로 잡히기** 때문이다. 비트
순서, 다섯 순열의 1-based 실수, 빠진 최종 스왑, 교차 진입, 서브키 순서는 전부
거의 모든 블록을 바꾼다. 그런데 깨진 `MAT_SBOX` 항목은 S1 의 64개 입력 중 하나에서만
발동하므로 한 블록이 그것을 건드릴 확률이 `1 - (63/64)^16 ≈ 22%` 이고, 64바이트
탐침이 통째로 놓칠 확률이 `(63/64)^128 ≈ 13%` 다. 128바이트면 약 1.8% 로 떨어진다.
지금 호스트는 그 테이블을 게스트 메모리에서 읽으니 틀릴 수가 없다 — 나중에 누가
하드코딩으로 바꿀 때를 위한 보험이다.

검사가 건드리는 것은 호스트가 할당한 힙 블록뿐이다 — 플래그는 **래퍼**에 있고
래퍼는 부르지 않는다. `nativeStartApp` 뒤에서 하는 이유는 `MX_desInit` 이
`onStartApp` 에서 돌기 때문이고, `KEY4ENCRYPT == 0xff` 면 아직 키 스케줄이
없다는 뜻이라 검사를 첫 실사용까지 미룬다.

**검사에 이가 있는지도 확인했다.** `MAT_SBOX[58]` 에 FIPS 값 3을 일부러 넣으면
양방향 모두 불일치를 잡아내고 `0 des` 로 떨어진다(그때 복호화 쪽은 첫 블록이
우연히 일치하므로 로그가 **첫 불일치 블록**을 찍는다 — 안 그러면 "같은 바이트가
다르다"로 읽힌다).

실측: 몬스터 로드 프레임 **202 ms → 48 ms**. `CS_NO_DES=1` 로 되돌린다.

## 네트워크 프로토콜 — 사설 서버까지 (2026-08-11)

원본 서버는 없어졌지만 게스트의 네트워크 코드는 온전하다. 목표는 게스트
바이너리를 **한 바이트도 고치지 않고** 우리 서버와 대화시키는 것이었고, 그래서
연결 리다이렉트는 호스트 쪽 한 곳(`net.rs::redirect_target`)에만 있다.

### 왜 소켓이 아예 안 열렸나

`편지` → "네트워크 연결 시 통화료가 부과됩니다" → `예` 를 눌러도 화면에는
"문제가 발생하여 접속종료 되었습니다"만 떴고, **호스트 로그에 `connect` 시도
자체가 없었다**. 추적한 호출 사슬:

```
openConnect → NTK_openConn → MX_netConnect → MX_smartConnect
  → _midas_funcLibConnect → MC_knlSetTimer(100ms)
  → _midas_funcLibConnectTimerCB → MC_netConnect(0xf8884)
      → AndroidService::isNetworkAvailable()
```

`MC_netConnect` 는 소켓을 만들기 **전에** `isNetworkAvailable()` 을 보고
`(ret-1) > 1`(부호 없음)이면 -1 로 즉시 반환한다. 즉 유효값은 `{1, 2}` 뿐인데
우리 HLE 는 0(네트워크 없음)을 답하고 있었다. -1 은 실패 콜백조차 부르지 않아서
(-10 만 콜백을 부른다) 게임 자체 연결 타임아웃이 돌 때까지 아무 일도 안 났다.

두 번째 벽은 안드로이드였다. 고친 뒤 기기에서
`connect to 127.0.0.1:8094 failed: Operation not permitted (EPERM)` 이 떴다 —
매니페스트가 "서버가 죽었으니 아무 권한도 요청하지 않는다"는 전제로 INTERNET 을
빼 두었기 때문이다. 사설 서버를 세우는 순간 그 전제가 무효가 된다.

### 요청 프레임 (C→S, 실캡처)

```
[u32 BE 전체길이(자기 포함)]
  +0   32B  tag    "AH_1.0.0"
  +32  16B  model  "SM-G991N"
  +48  12B  os     "ANDROID"
  +60  44B  ver    "2.X"
  +104 ..   body = [u8 len(자기 포함)][u32 BE opcode][payload]
```

길이가 정확히 맞아떨어진다: 130B 프레임의 길이 필드 = 130, body `0x16`(22B);
114B 프레임 = 114, body `0x06`(6B). 빅엔디안인 것은 게스트가
`processingPacketPut{Short,Int,Long}_HOST` 로 채우기 때문이다.

관측된 body 는 둘 — `16|0x70|01 30 …`(접속 직후 1회, 핸드셰이크)과
`06|0x01|01`(5초 주기, `sendAlivePacket`).

### 응답 프레임 (S→C) — 요청과 형식이 다르다

여기서 오래 막혔다. 응답을 보내도 `netLibCB` 에 수신 이벤트(10)가 오지 않았다.
`CS_NET_DEBUG=1` 로 게스트가 실제로 요청한 바이트 수를 찍자 즉시 드러났다:

```
recv want=1    got=1: 00              ← 선두 1바이트를 먼저 읽는다
recv want=4    got=4: 00 00 0c 00     ← 이걸 길이로 해석 = 0x00000c00 = 3072
recv want=3068                        ← 3072-4 를 기다린다 (영원히)
```

우리 응답이 **정확히 1바이트 어긋나** 있었다. 형식은:

```
[u8 선두][u32 BE len][body]           body 는 정확히 len-4 바이트
  body = [4B ?][u8 cat][u8 sub][u16 BE status][payload]
```

소켓 계층이 길이 4바이트를 벗겨내고 body 만 올리므로, `ProcRecvPacket` 이 보는
카테고리/서브는 **body 오프셋 4/5** 다. `body <= 5` 는 버려지고, `cat` 0x70 은
`ProcRecvPacket_Single`, 0x64 는 `ProcRecvPacket_PATCH` 로 간다. `status != 0`
이면 에러 팝업 경로. `sub` 점프테이블이 처리하는 값은 2, 4, 6, 19, 23, 25, 27.

1바이트를 붙이자 그대로 통했다:

```
recv want=1 got=1: 00
recv want=4 got=4: 00 00 00 0c
recv want=8 got=8: 00 00 00 00 70 02 00 00
netLibCB(0xa, 0x8, …)          ← 수신 이벤트
ProcRecvPacket_Single(0x2, …)  ← 디스패치
```

기기에서 로딩 스피너가 풀리고 캐릭터 슬롯 목록까지 진행했다. 클라이언트
네트워크 상태 바이트는 게스트 `0x1172f59` 에 있고 `cs_probe --peek` 로 읽는다
(관측값 `0x0b`; `sub=2` 핸들러가 0xa~0xd 상태에서만 동작한다).

### 남은 것

* **캐시상점** — 프로토콜 해독 + 서버 구현 완료(2026-08-10), 실기기 검증 대기.
  아래 별도 절 참고.
* **환경설정 → 데이터 관리** — 조사 완료(2026-08-11), 아래 별도 절 참고. 남은
  일은 `11536` 포트 프로토콜 해독과 `cs_server` 구현뿐이다.

### 데이터 관리 (2026-08-11)

`MainMenuFrame_drawDataManage`(0xb46b8, 3156 B). 기기변경/번호변경 시 세이브를
서버에 맡겼다가 되받는 기능이고, 화면 흐름은 확인창 두 번 → 전송 → 결과다.
`cs_probe` 로 결정적으로 재생된다(S25 구성 기준):

```powershell
cargo run --release -p cs_probe -- --w 2340 --h 1080 --wipi 520x240 `
  --no-integer-scale --fill-width --frames 5000 --ms 33 `
  --taps "60:1301x605,110:1169x677,450:1170x540,550:1170x544,600:1170x544,950:1169x807,1100:1036x807" `
  --keys "650:DOWN,670:DOWN,690:DOWN,710:DOWN,730:DOWN,750:DOWN,770:DOWN,790:DOWN,810:DOWN,830:DOWN,880:OK"
```

메뉴 타일은 **한 번 탭해서 커서 이동, 한 번 더 탭해서 실행**이다. 첫 확인창의
「예」는 화면 중앙(1169x807)이지만 둘째 확인창은 예/아니오 두 개라 「예」가
왼쪽(1036x807)으로 옮겨간다 — 중앙을 누르면 두 버튼 사이 빈 곳이라 아무 일도
안 일어난다.

게스트는 `210.222.18.28:11536` 으로 connect 한다. `redirect_target` 이 이것도
`cs_server` 로 보내지만 그쪽엔 핸들러가 없어 TCP 만 열리고 응답이 없다 —
`CS_NET_DEBUG=1` 에 `recv` 가 한 줄도 안 찍힌다. 게스트가 자체 타임아웃 뒤
"서버와 접속이 원활하지 않습니다" 를 띄우고, 그 팝업의 「예」에서
`CletEntry.exitApp()` 로 간다(프로브 로그: `guest exited at frame 2601`).

**세이브는 손상되지 않는다.** "서버에 저장된 게임은 초기화 됩니다" 는 업로드가
성공한 뒤의 이야기라 여기까지 오지 않는다. 전후 `kickass` 1060 B 가 바이트 단위로
동일하다(MD5 `e7da28e54bd55191ef075adc97c3b55c`).

"그 뒤로 앱이 아예 안 켜진다"는 별개의 **호스트** 버그였고 고쳤다 — `exitApp`
이후 프로세스가 살아남아 다음 실행이 `EventLoop` 재생성으로 패닉하던 문제.
`CLAUDE.md` 의 해당 절 참고.

#### DSP CrossRoad 프로토콜 (2026-08-11)

데이터 관리의 전송 계층은 **DSP CrossRoad** 다. `.so` 가 스트립되지 않아 상태기계
전체가 이름으로 남아 있다: `MX_dspCrossRoad{Init,Connect,Close}`,
`_midas_funcDspCrossRoadMakePacket`(0x30554), 여섯 개의 `Send*Query`, 그리고
응답을 해석하는 `_midas_funcDspCrossRoadLibCb`(0x37400).

**요청** — `MakePacket(cmd_str, kind)` 이 만든다. 앞 104바이트는 메인 프로토콜과
같은 단말 프로필이라 `server.py` 의 `HDR`/`BODY_AT` 가 그대로 맞는다(실측
확인). body 는 ASCII:

```
<명령> ' ' <MC_knlGetSystemProperty 값> ' ' <게임ID=G1000148> [' ' <추가>]
```

가운데 시스템 속성은 원본에서 **가입자 식별자**였고 우리 HLE 는 빈 문자열을
준다 — 그래서 실측 body 가 `START_SAVEDATA_UP  G1000148 1060` 처럼 **공백이
둘**이다. 서버는 이 칸을 계정 키로 써야 한다(비어 있으면 단일 슬롯).

kind 는 `.rodata` 0x104398~0x104408 의 명령 문자열과 1:1 이고, **응답의 첫
바이트(cmd)가 곧 이 kind** 다:

| kind | 명령 | 보내는 함수 | 추가 필드 |
|---|---|---|---|
| 2 | `IS_SAVEDATA_EXIST` | `SendDnloadCheckQuery` | 없음(뒤에 공백 1) |
| 3 | `BLOCK_SAVEDATA_DOWN_OK` | `SendDnloadQuery` | 없음(뒤에 공백 1) |
| 4 | `START_SAVEDATA_UP` | `SendUploadCheckQuery` | `MC_fsFileAttribute` 로 얻은 **파일 크기** |
| 5 | `BLOCK_SAVEDATA_UP` | `SendUploadQuery` | 공백 1 + `sprintk` 수치 + `fsSeek`/`fsRead` 로 읽은 **청크 원본 바이트** (패킷 버퍼 0x440) |
| 6 | `DELETE_SAVEDATA` | `SendDeleteQuery` | 없음 |
| 7 | `FINISH_SAVEDATA` | `SendFinishQuery` | 없음 |

**응답** — 메인 프로토콜(`[u8 선두][u32 BE len][body]`)과도, 캐시상점
(`[u32 BE len][body]`)과도 **다르다.** 바깥 길이 접두가 아예 없다:

```
[u8 cmd][u8 status][u8 n][n 바이트][cmd 별 추가]
```

`_midas_funcDspCrossRoadLibCb` 가 총 길이를 계산해 모자라면
`MX_netSocketReadContinue` 로 더 기다린다. cmd 별 총 길이:

| cmd | 추가 필드 | 총 길이 |
|---|---|---|
| 1, 4, 6, 7 | 없음 | `n+3` |
| 2 | `[u8 존재여부][u32 BE 저장크기]` | `n+8` |
| 3 | `[u32 BE datalen][datalen 바이트]` | `n+7+datalen` |
| 5 | `[u32 BE offset]` | `n+7` |

* `status`(byte 1)는 `strb` 로 **제로확장**되어 워드에 들어간다 — 즉 코드의
  `status < 0` 분기는 와이어로는 절대 타지 않는다. 내부 오류 전파용이다.
* cmd 2 의 **존재여부 바이트가 0 이면** `CallImmediately(2, -1)`, 0 이 아니면
  `(2, 0)` 이다. 세이브 유무는 status 가 아니라 이 바이트로 답한다.
* cmd 3 은 `datalen <= 0` 이면 다운로드 종료로 보고, 그렇지 않으면
  `MC_fsOpen`/`MC_fsSeek`/`MC_fsWrite` 로 파일에 이어 쓴다. 끝나면
  `MC_fsFileAttribute` 로 **받은 크기와 기대 크기를 비교**해서 다르면
  `MC_fsRemove` 로 지운다.
* cmd 5 는 status 확인 후 전역 두 개(누적 오프셋 / 전체 크기)를 비교해서
  같으면 **로컬 세이브를 `MC_fsRemove` 로 지우고**(= "서버에 저장된 게임은
  초기화 됩니다") `CallImmediately(4, 0)`, 다르면 다음 블록을 보낸다.

**그래서 업로드만 구현하면 안 된다.** 업로드 성공의 마지막 단계가 로컬 세이브
삭제이므로, `IS_SAVEDATA_EXIST` + `BLOCK_SAVEDATA_DOWN_OK` 다운로드 경로가 같이
동작하기 전에는 실기기에서 이 메뉴를 쓰면 세이브가 사라진다.

### 캐시상점 프로토콜 (2026-08-10)

클라이언트 UI 는 전부 살아 있다(`npcUI_cashSel`, `NpcUI_setBoughtItemOnCASH`,
`GameMenuFrame_setPayCASHForRebirth/OpenOldBox`, `Charge_makeData`,
`CHARGE_BUYABLE`/`CHARGE_ALL`). 디스어셈으로 확정한 것:

* **아이템 목록은 네트워크가 아니라 VFS 다.** `NpcUI_isCashTem`(0xe19c8)이
  `NPC/MERCHANT_<id>` 를 `UM_LoadRes` 로 읽어 `cashTemType` 배열을 채운다.
  MERCHANT_0..3·MERCHANT_H 가 VFS 에 실재한다. 서버는 목록에 관여하지 않는다.
* 캐시상점은 메인 게임 서버가 아니라 **스마트요청 소켓**을 탄다.
  `Charge_makeData`(0xd1cdc)가 `"%s %s %s %s%d %s"` = `G1000148 C <전화>
  CHARGE_BUYABLE<n> <모델>` 을 `MX_netSocketWrite` 로 보낸다. 인자 `r8`:
  3=CHARGE_BUYABLE(구매가능 질의), 4=CHARGE_ALL(구매 실행).
* 응답은 `_midas_funcOnSmartReqSocketReadCB`(0x366bc)가 `[u32 BE 전체길이]`
  헤더를 먼저 읽고 `len-4` 바이트 본문을 `netLibCB(0xa, len, body)` 로 올린다.
  **메인 프로토콜의 선두 1바이트가 없다.** `netLibCB`(0xd289c) 는 본문을
  `netSocket+0xbc` 로 복사하고 `body[0]`(상태)를 `isGameServer` 로 분기한다:
  * `isGameServer==3` (0xd2ab0): `body[0]==0x11` → `isGameServer`를 3→4로
    바꾸고(`strb #4,[&isGameServer]`) `Charge_makeData(4)` 전송; `==1` 대기;
    그 외 → 끊고 에러(`nameTitleIdx=0xc8`).
  * `isGameServer==4` (0xd29b0): `body[0]==0` → 성공 → NpcUI 상태에 따라
    `NpcUI_setBoughtItemOnCASH`/`setPayCASHForRemoveOrb`/`GameMenuFrame_
    setPayCASHFor{OpenOldBox,Rebirth}` 중 하나로 아이템 지급 후 오토세이브.
    그 외 → 끊고 에러. **지급 아이템은 클라의 MERCHANT 선택이 정하므로**
    (`NpcUI_setBoughtItemOnCASH` 가 `_merchant + netSocket[0xb4]` 를 읽는다)
    이 응답에는 성공 상태 바이트만 있으면 된다.
* `ProcRecvPacket_Single`(0xa6720)의 sub 점프테이블은 메인 게임 서버용이며
  2,4,6,19,23,25,27,33 만 처리한다(sub 4 = 캐릭터 슬롯, sub 19/23/27 = 편지함
  `_mailBox`, sub 25 = 편지+아이템). 캐시 지급은 이 경로가 아니라 스마트요청
  `netLibCB` 경로다 — 위 참고.

`cs_server/server.py` 가 스마트요청 프레이밍과 두 왕복(`CHARGE_BUYABLE`→0x11,
`CHARGE_ALL`→0x00)을 구현했고, 헤드리스 셀프테스트(로직 + 실 asyncio 소켓)로
검증했다. 실기기 종단 검증은 네트워크가 붙는 상태에서 한다.
* **TLS + 핀 고정** — 서버는 `--tls` 로 켤 수 있지만 게스트 쪽 클라이언트에
  지원이 없어 아직 평문이다. 포트를 연 지 몇 분 만에 스캐너가 붙는 것을 봤다.

### 아이템 설명 팝업은 화면 아래에 붙는 것이 원본이다 (2026-08-11)

캐시상점에서 아이템을 고르면 설명 상자가 좌하/우하에 **딱 붙어** 나오고,
가방에서는 좌중/우중처럼 보인다. 버그가 아니다 — 두 화면이 **같은 규칙**을
쓰고 팝업 **높이만** 다르다.

`NpcUI_get{PopupX,PopupY}`(0xd4fd8/0xd5030)와 `get{X,Y}_IGM_Popup`
(0xa56dc/0xa5734)은 **명령어 단위로 동일한 함수 두 벌**이다. 인자는
`(sel/4, sel%4, w, h)` — 4열 격자의 행·열이다:

```
x = (열 <= 1) ? _lcdG[0x4c] - w : 0        // 행 0..5, 아니면 0
y = (행 <= 2) ? _lcdG[0x50] - h : 0
```

`_lcdG+0x4c/+0x50` 은 논리 화면 폭·높이다(같은 함수들이 `asr #1` 로 중앙을
잡는 데 쓴다). 즉 **가로는 고른 칸의 반대쪽 화면 끝, 세로는 화면 아래 끝**에
정렬한다. 폭 533 에서도 `533-158=375` 로 오른쪽 끝에 붙으므로 와이드 때문에
생긴 차이가 아니다(세로는 어느 구성에서도 240 그대로다).

높이가 전부다:

| 화면 | 팝업 | y (240 기준) |
|---|---|---|
| 가방 / 상점의 **내 물건** 쪽 | 158 x **167** | 240-167 = **73** |
| 캐시상점 / 상점의 **파는 물건** 쪽 | 158 x **122** | 240-122 = **118** |

167 짜리는 위로 73까지 올라와 "가운데"로 읽히고, 122 짜리는 118 부터라 아래에
붙어 보인다. 원본 32비트 APK 를 나란히 놓고 확인하는 것이 최종 정답지지만
(`CLAUDE.md` 의 그 절), 계산이 화면 크기와 무관하게 닫혀 있어 확인 없이도
호스트 쪽 원인이 아님이 나온다.

### 캐시상점을 공짜로 (2026-08-11)

`CS_FREE_CASH`(기본 켜짐). 구현과 주의점은 `CLAUDE.md` 의 해당 절에 짧게 있고,
근거가 되는 사실만 여기 남긴다:

* 가격은 `_merchant + 0x106c + idx*4` 의 `i32[100]` 이다(`idx = bagSelCnt*20 +
  _BagSlotIdx`). `NpcUI_initMerchantData` 가 -1 로 채우고, 골드 가격과 캐시
  가격이 **같은 배열**을 쓴다. 캐시 쪽만 `merchantState == 3` 분기에서
  `"|CASH &%d"` 로 그린다.
* 그래서 데이터를 0 으로 덮는 대신 **그리는 지점 하나만** 후킹했다 — 원가를
  보여줘야 하고, 골드 상점을 건드리면 안 되기 때문이다.
* `MX_smartSocketWrite` 는 108바이트 헤더와 100바이트 본문을
  `_midas_internalQueue` 에 넣고 나중에 한 번에 흘린다. 그래서 마커 검색은
  고정 오프셋이 아니라 **버퍼 스캔**이어야 한다(`cs_core` 의 테스트가 이
  모양을 그대로 만들어 확인한다).

### 정정: 캐시 결제는 소켓이 아니라 JNI 다 (2026-08-11, 같은 날 늦게)

바로 위 절과 `cs_server` 의 캐시상점 구현은 **프로토콜로서는 맞지만 이 안드로이드
빌드에서는 도달하지 않는다.** `Charge_makeData(3)` 은 `isGameServer == 3` 에서만
불리는데, `.text` 전체의 리터럴 풀을 훑어 `isGameServer`(0x172f58) 를 쓰는 16곳을
모두 뽑아 보면 저장되는 값은 `0/1/2/5/0xa/0x32` 뿐이다 — **3 은 없다.**

실제 경로:

```
NpcUI_merchantSelectKeyPressed  netSocket+0x17,+0x18 = 1
  -> MainFrame_setNopenChargeOnCASH (0xa5da4)
       netSocket+0xb4/+0xac/+0xb0/+0xb8, _ConnectCnt = 0,
       isGameServer = 5, netSocket+0x15 = 1,
       MX_mtxSetCB(MTX_Result_CB, 0)
  -> MX_mtxRequest (0xff72c)   *mMTXCB 검사, *bProgress = 1
  -> AndroidService::requestMTX (0xfe480)   순수 JNI CallStaticIntMethod
```

즉 원폰에서는 **통신사 결제창을 플랫폼이 띄웠고**, 게임은 콜백만 기다린다.
`hle.rs` 가 이 임포트를 -1 로 답하고 콜백을 부르지 않았으니 위 네 플래그
(`netSocket+0x15/+0x17/+0x18`, `bProgress`)가 1 로 남고, 상점 키 핸들러가 그
상태에서 입력을 안 받는다. **"구매하면 무한 로딩"의 정체가 이것이다** — 서버가
답을 안 해서가 아니었다.

`MTX_Result_CB`(0xa5ba4) 규약은 디스어셈으로 확정:

* `r1 == 0` → 성공. `r1 != 0` 이면 `r2 == -10` 이 사용자 취소(문자열 0x303),
  그 외는 0x22b("서버와 접속이 원활하지 않습니다").
* `r0` 은 **양쪽 경로에서 쓰이기 전에 덮어쓰인다** — 그래서 콜백을 합성할 때
  넘길 값이 없다.
* 성공 경로는 `NpcUI_getNpcUIState()` 로 갈라져 `NpcUI_setBoughtItemOnCASH` /
  `setPayCASHForRemoveOrb` / `GameMenuFrame_setPayCASHFor{Rebirth,OpenOldBox}` 를
  부르고 오토세이브·`disconnect(1)` 까지 게스트가 한다.

그래서 호스트가 할 일은 `bProgress` 를 지우고 콜백을 `(_, 0, 0)` 으로 부르는 것
뿐이다(`hle.rs::complete_mtx`). 지급 대상은 전적으로 게스트가 정한다.

부수 효과로 **결제 지점 네 곳이 전부 공짜가 된다**(캐시상점·오브 제거·즉시
부활·헌 상자). 넷 다 같은 콜백을 쓰고, 서비스가 없어진 이상 "공짜" 아니면
"영영 고장" 뿐이라 그대로 두었다.

### "통화료 부과" 문구는 캐시상점에 없다

775개 in-code 문자열을 전부 디코드하고 `.rodata`/`.data`/`.text` 를 EUC-KR 로
따로 훑어 확인했다 — 통화료/부과/요금/과금/결제 를 담은 문자열은 47개고 **전부
in-code 표 안에** 있다(표 밖 pc-relative 문자열은 없다).

* 캐시상점 확인창(`NpcUI_DrawMerChant_DealPopup`)이 쓰는 id 는
  0x2eb/0x2ec/0x1f/0x20/0x2ed/0x2ee/0x2ef/0x2ea 뿐 — `구입 하시겠습니까?` 와
  예/아니오, 구입갯수, 그리고 캐시 가격. **금액·요금 문구가 없다.**
* "(소량의 통화료 부과)" 는 **환경설정 → 데이터 관리** 것이다. 본문 문자열은
  `MainMenuFrame_drawDataManage` 가 0x217..0x221 을 스택 배열에 캐시해 두고
  `dataMgrState`(0x131f40)로 인덱싱한다.
* 그 11개 블록의 **문구만 다른 복제본이 `id + 0xb` (0x222..0x22c) 에 통째로
  들어 있고 아무도 참조하지 않는다** — 끝의 괄호가 "(정보료/통화료 무료)" 다.
  11개 중 6개는 아예 같은 포인터이고, 다른 5개는 그 괄호만 다르다(문자열 단위로
  대조 확인). 그래서 `getInCodeString` 에서 id 하나를 옮기는 것으로 끝난다.
* 실제 현금 액수를 말하는 확인창은 따로 셋이다: 헌 상자(0xa9),
  오브 제거(0x300, `NpcUI_drawOrbRemovePayment`), 그리고 즉시 부활이 가져오는
  0xb9 — 마지막 것은 포인터가 쓰이기 전에 덮어써져서 **화면에 안 나온다**.

### 메뉴 좌측하단은 화살표가 아니라 가상 D-pad 다

`CLAUDE.md` 의 해당 절에 사실만 정리했다. 여기 남길 것은 방법과 함정:

* 이름으로 찾으면 못 찾는다. `ArrowMvoe_draw`·`ArrowMvoe_draw_LR`·
  `ArrowMvoe_drawUpDown`·`Arrow_drawUpDown` 은 전부 4바이트 `bx lr` 스텁이고,
  `Draw_Arrow` 는 환경설정 값 좌우의 화살표다. 실제 D-pad 는 **다섯 화면에
  인라인 복붙**돼 있어 공용 심볼이 없다.
* 찾는 신호는 "x 가 작은 상수 + y 가 `*(_lcdG+0x50)` 에서 작은 상수를 뺀 값".
  `bl DM_DrawImgRotateEx` 229곳을 전부 디코드해 각각의 `getImage` 프레임까지
  거슬러 올라가면 `_imgGameUI` 프레임 6 을 그리는 곳은 26곳, 그중 화면 모서리에
  붙는 메뉴용이 18곳이다.
* 스프라이트는 **27x16** 이다. 28 은 `DM_CreateImage` 가 `[desc+0]` 에 넣는
  4바이트 정렬 폭(소스 스트라이드용)이고, 호출자가 `r3` 로 넘기는 폭은
  `[desc+8]` = 27 이다. GBM 헤더의 int16 네 개(28/16/27/15)와
  `39 + 14*16 = 263` = 레코드 길이가 서로 검산된다.
* **터치 판정은 그림과 별개다.** `Check_Rect` 4연발이 키 핸들러 약 스무 곳에
  복붙돼 있고, 사각형이 스프라이트보다 크며 모서리를 빈틈없이 타일링한다
  (H=240 에서 x 0..91, y 157..240). 그림만 옮기면 보이는 곳과 눌리는 곳이
  어긋나고 `Check_Rect` 안의 `SM_Vibrator` 도 옛 자리에서 운다.
