# 캐시 비약 3종 + 버프 HUD — 실현 가능성 분석과 구현 설계

> 작성 2026-08-12. **구현 전 분석 문서**다. 여기 적힌 주소·오프셋·명령어 워드는
> 전부 `payload/libchronosMian.so` 에서 직접 확인한 것이고, 확인 방법은 부록 D.
> "확인됨"과 "설계(미검증)"를 문단마다 구분해 두었다 — 다음 세션이 그 경계를
> 믿고 작업할 수 있어야 하기 때문이다.
>
> **구현 완료 2026-08-13** (`cs_core/src/potions.rs` + `patch.rs`). 설계와 달라진
> 곳과 이 문서가 틀렸던 곳은 **§9** 에 모아 두었다 — 먼저 읽을 것. 요약: 요구레벨
> 게이트는 7곳이 아니라 **9곳**이고, `NpcUI_setBoughtItemOnCASH` 는 호출자가
> **둘**이며, HUD 는 안1·안2 중 하나가 아니라 **둘 다** 필요하다.
>
> **이 문서는 세 개만 다룬다.** 네 번째 **작명의 비약**(0x23/0x13, 2026-08-15)은
> 여기 없다 — 타이머가 아니라 즉발이라 `PotionState` 밖에 있고, 그래서
> `Shelf.kind: Kind` 가 `Shelf.effect: Effect`(`Timed(Kind)` | `Rename`)로 바뀌었다.
> 전문은 **`CLAUDE.md` 「펫 작명」절**.

## 0. 요청

1. 캐시상점에 **없던 아이템 3종**을 추가한다. **셋 다 지속시간은 일주일.**
   * **성장의 비약** — 획득 경험치 2배, 7일
   * **슈퍼성장의 비약** — 획득 경험치 4배, 7일
   * **종결자의 비약** — 장비 요구레벨 -5, 7일
2. **메뉴를 열었을 때 HP/SP HUD 오른쪽**에 지금 계정에 걸린 비약 버프를
   **한글로** 보여준다.

> 일주일이라는 값이 설계를 하나 바꾼다: 버프는 **앱이 꺼져 있는 동안에도 흘러야**
> 하므로 시간 기준이 `Vm::now_ms()`(프로세스 상대)가 아니라 **벽시계**여야 한다.
> 3.7 에서 그 결과를 다룬다.

## 1. 결론

**전부 가능하다. 게스트 바이너리를 고칠 필요도, `data.vfs.mp3` 를 다시 만들
필요도 없다.** 호스트 패치 일곱 개(설치 지점 13곳)와 사이드카 상태 파일 하나로
끝난다. 새 crate 의존성은 없다.

| 조각 | 훅 | 설치 지점 | 난이도 | 위험 |
|---|---|---|---|---|
| 상점 목록에 3종 추가 | `NpcUI_setMerchant` 진입 | 1 | 낮음 | 없음 (게스트가 파싱을 끝낸 뒤 배열에 덧붙임) |
| 구매 시 효과 적용 | `NpcUI_setBoughtItemOnCASH` 진입 | 1 | 낮음 | 없음 (단일 진입점, 호출자 1곳) |
| 경험치 배수 | `HERO_add_Exp` 진입 | 1 | **아주 낮음** | 없음 (인자 하나 곱하고 decline) |
| 요구레벨 -5 | `Check_*` 6개 + 펫 인라인 1곳 | 7 | 중간 | 7개 지점 전수 처리 필요 |
| 요구레벨 표시 | `drawItemInfo+0xc6c` | 1 | 낮음 | 안 하면 툴팁과 실제가 어긋나 보인다 |
| 버프 HUD | `Draw_PlayInfo` (+옵션 `IGM_DrawMenu`) | 1~2 | 중간 | EUC-KR 바이트를 직접 써야 함 |
| 영속성 (7일, 벽시계) | `Config::storage` 아래 사이드카 | 0 | 낮음 | 게스트 세이브를 건드리지 않음. 단 시각원을 `wall_now_ms()` 로 새로 둬야 함 |

특히 중요한 발견 두 가지:

* **캐시 아이템은 "가방에 들어가는 아이템"일 필요가 없다.** 구매 완료는
  `NpcUI_setBoughtItemOnCASH`(0xd6654) 한 곳으로 수렴하고, 그 함수가
  `ItemBag_checkAndSetItem` 을 부르기 **전에** 가로채면 아이템 DB도, 아이콘도,
  사용 핸들러도 전혀 건드리지 않는다. 즉 오토루팅과 같은 부류의 "즉시 효과"로
  만들면 게스트가 모르는 아이템 id 를 만들 필요가 없다.
* **상점이 그리는 것은 전부 `_merchant` 전역 배열에서 온다.** 이름·설명·가격·
  등급색은 VFS 파일에서 온 값이 그 배열에 들어가 있을 뿐이고, 아이콘만 아이템
  id 로 고른다. 그래서 **기존 id 를 재사용하고 subType 만 새로 주면** 아이콘이
  공짜로 따라온다.

## 2. 확인된 사실

### 2.1 캐시상점 목록은 `NPC/MERCHANT_2` 하나뿐, 그리고 `_merchant` 로 파싱된다

`NpcUI_setMerchant`(0xe20a4, 0x2e8) 이 `MC_knlSprintk("NPC/MERCHANT_%d", n)` →
`UM_LoadRes` → 레코드 루프. 파싱 결과가 앉는 자리(**전부 확인됨**, 배열 경계가
`_merchant` 크기 0x7660 과 정확히 맞물린다):

| 필드 | 오프셋 | 타입 | stride | 개수 |
|---|---|---|---|---|
| 개수 | `_merchant + 0` | u16 | — | — |
| 아이템 id | `_merchant + 0x02` | s8 | 1 | 100 |
| subType | `_merchant + 0x66` | s8 | 1 | 100 |
| 이름 | `_merchant + 0xca` | EUC-KR | 0x28 | 100 |
| 가격 | `_merchant + 0x106c` | s32 | 4 | 100 |
| 수량 | `_merchant + 0x11fc` | u8 | 1 | 100 |
| 설명 | `_merchant + 0x1260` | EUC-KR | 0xff | 100 |
| 등급색 | `_merchant + 0x75fc` | u8 | 1 | 100 |

`0x1260 + 99*0xff + 0xff = 0x75fc` 이고 `0x75fc + 100 = 0x765c ≤ 0x7660` —
**설명 stride 가 0xff 이고 최대 100개**라는 것이 배열들이 서로 맞닿는 방식으로
증명된다. `_merchant` 는 `.bss` 의 0x173390.

파싱 뒤 `_merchant + 2 + count` 부터 `+0x66` 까지 **0xff(=-1) 종결자**로 채운다
(0xe22d0 루프). 그리고
`NPCUI_BAG_MAX(0x17a9f3) = ceil(count / 20)` — 20 = 4열 x 5행.

파일 포맷(재확인, 768/768 바이트 완전 소비):

```
u16 count
count x { u8 id, u8 subType, s16 nameLen, name[nameLen],
          s32 price, u8 qty, s16 descLen, desc[descLen], u8 gradeColour }
```

**CLAUDE.md 의 "descLen ≤ 0x27" 은 틀렸다.** 출고 파일에 이미 56/52/66바이트
설명이 들어 있다. 실제 구조는 `memset(buf, 0, 0x28)` 뒤 `memcpy(buf, src, descLen)`
이고 버퍼 stride 는 0xff 다. 즉 0x28 을 넘는 설명은 **뒤가 이미 0이라는 데
기대는** 출고 버그이고(`.bss` 초기 0 + 같은 슬롯에 더 긴 설명이 먼저 들어온 적이
없음), 값 자체는 254바이트까지 버퍼 안이다. 호스트가 직접 쓸 때는 NUL 을 같이
쓰면 되므로 제약이 사라진다.

### 2.2 캐시상점을 여는 것은 `merchantState = 3; NpcUI_setMerchant(2)` 다

두 곳에서만 그렇게 연다(**확인됨**):

* `GameFrame_PressTouchKeypad+0x760` (0x60a4c)
* `GameFrame_gamekey+0x4ec` (0x61184)

둘 다 `strb #3, [merchantState]` 직후 `mov r0,#2; bl NpcUI_setMerchant`.
`NpcUI_merchantKeyPressed+0xa98`(0xe2e24)도 `merchantState = arg + 1` 규약으로
같은 함수를 부르므로, **`r0 == 2 && merchantState == 3`** 이 "캐시상점"을 정확히
집는 조건이다. 마을 상점은 `setMerchant(0/1/3)` → `merchantState 1/2/4`.

> `NpcUI_setMerchant(3)` 은 파일을 읽지 않고 `NPCUI_BAG_MAX = _storage[2]` 만
> 세우는 **판매 화면**이다(0xe2330). 인자 3 과 상태 3 을 헷갈리면 안 된다.

### 2.3 아이콘은 아이템 id 로만 고른다

`NpcUI_DrawMerChant_Item`(0xe05c8), `merchantState <= 3` 가지:

```
r1 = (s8)_merchant[2 + page*20 + i]      ; 아이템 id
r0 = *_imgITMIcon
sb = getImage(r0, r1)                    ; getImage(base, i) = base + i*0x28
w  = Item_getIconWidth(id)  = sb[8]
h  = Item_getIconHeight(id) = sb[4]
DM_DrawImgEx(g, x, y, sb[8], sb[4], sb, 0,0,0,0, 0x4c, 7, 9)
```

`getImage`(0xe6a10)는 3명령짜리 `base + index*0x28` 이다(**확인됨**). 그래서
**id 는 아이콘 인덱스**이고, subType 은 아이콘에 아무 영향이 없다.

이름/설명/가격은 `NpcUI_DrawMerChant_Info`(0xe0c38)가 `merchantState <= 3` 가지
(0xe0db0)에서 `_merchant + 0xca + (page*20 + sel)*0x28` 을 직접 읽는다 —
**아이템 DB 가 아니라 상점 파일**에서 온다(**확인됨**).

### 2.4 구매는 `NpcUI_setBoughtItemOnCASH` 한 곳으로 수렴한다

이 빌드에서 캐시 구매의 전 경로(**확인됨**, `CLAUDE.md` 의 "캐시상점은 소켓을
안 쓴다" 절과 일치):

```
예(2패스) → NpcUI_merchantSelectKeyPressed+0x328 (merchantState==3 가지)
   netSocket[0x18] = 1
   ItemBag_leftSlotCntOnAllBag() > 0 확인          ← 가방이 꽉 차면 여기서 거절
   netSocket[0x17] = 1
   tail → MainFrame_setNopenChargeOnCASH(page*20+sel, qty, 3)
        netSocket[0xb4] = 슬롯 인덱스
        netSocket[0xac] = MainFrame_getItemContentsID(slot)
        netSocket[0xb0] = MainFrame_getItemContentsRealID(slot)
        isGameServer = 5, MX_mtxSetCB(MTX_Result_CB), MX_mtxRequest
   → AndroidService::requestMTX  ← HLE. hle.rs::complete_mtx 가 성공으로 답한다
   → MTX_Result_CB(param, 0, 0)                    (0xa5ba4)
        NpcUI_getNpcUIState() == 2 → NpcUI_setBoughtItemOnCASH()   (0xa5c50)
```

`NpcUI_setBoughtItemOnCASH`(0xd6654, 0xb4바이트) 전문:

```
r0 = netSocket[0xb8]                       ; 수량
if (NpcUI_merchantGetCanPickup(r0) == 0) { errIdx = 0; return; }   ; 슬롯 부족
rec = _merchant + netSocket[0xb4]
NameTitle_checkCashItemNGet((s8)rec[2], (s8)rec[0x66])             ; 업적
dealCnt = 1; errIdx = 6; netSocket[0x18] = 0; dealSel = 0; npcUI_bDeal = 0
tail → GameFrame_autoSave()
```

`NpcUI_merchantGetCanPickup`(0xd644c)이 실제 지급이고, 하는 일은
`ItemBag_checkAndSetItem(_storage, (s16)rec[2], (s16)rec[0x66], cnt, 1)` 뿐이다.
**즉 캐시 아이템 = (id, subType) 쌍으로 가방에 들어가는 평범한 아이템**이고,
효과는 나중에 가방에서 "사용"할 때 난다.

**1패스(예를 처음 누를 때)가 무엇을 검사하는지**도 확인해 두었다. 캐시 경로는
`NpcUI_merchantSelectKeyPressed+0x78`(0xe1b4c)의 `merchantState == 3` 에서
0xe1d60 으로 갈라졌다가 곧장 0xe1b84 로 되돌아온다 — 즉 **비캐시 경로의
`ITEM_getItemFrom`(플레이어 가방 조회)을 건너뛴다**. 남는 검사는 둘뿐:

* `id == 0x3e`(확장가방)면 `_storage[2] > 4` 인지 보고 `errIdx = 0x10`
  ("더이상 확장할 수 없습니다") — **우리 id 는 0x23 이라 해당 없음**.
* 그 외에는 `sb == 0 && (id << 24) == 0xff000000`, 즉 **슬롯이 종결자(0xff)인지**
  만 본다. `sb` 는 이 가지에서 `npcUI_bDeal`(1패스에는 0)이다.

통과하면 `npcUI_bDeal = 1`, `dealSel = 0`. **새 항목은 실제 id 를 가지므로
1패스를 그대로 통과한다** — 3.1 이 id 를 새로 만들지 않는 또 하나의 이유다.

`errIdx`(0x131fed)는 `NpcUI_DrawErrorMsg`(0xd759c)가 **오프셋 없이**
`getInCodeString(errIdx)` 로 그린다(**확인됨**, 0xd7638). 그래서 `errIdx = 6` 은
id 6 = `구입하였습니다`, `errIdx = 0` 은 `슬롯이 부족합니다`, `errIdx = 1` 은
`소지금이 부족합니다`. 인게임 메뉴 쪽 `IGM_drawErrMsg`(0x64484)는 같은 전역에
`+0x88` 을 더해 쓴다 — 화면마다 베이스가 다르니 값을 그대로 흉내내면 된다.

### 2.5 사용 가능한 아이템 id 는 0x14..0x3e 뿐이다

`CheckUse_Item`(0xf0640):

```
if ((u8)(id - 0x14) > 0x2a) return 0;
return table[0x10bd34 + (id - 0x14)];      ; 0x2b바이트
```

표 값(0=사용불가, 1/2/3/4=부류). 호출자는 `GameMenuFrame_drawItemBag` 2곳 +
`GameMenuFrame_KeyPressed` 5곳. **이 범위 밖의 새 id 를 만들면 가방에서 쓸 수
없다** — 3종을 "가방 아이템"으로 만들지 않는 또 하나의 이유.

출고 캐시 목록 12종의 id 는 `0x2a 0x23 0x23 0x13 0x3e 0x14 0x15 0x28 0x28 0x28
0x35 0x35` 다(전문은 부록 B).

### 2.6 경험치는 `HERO_add_Exp` 한 곳으로 들어온다

`HERO_add_Exp(r0 = heroIdx, r1 = amount)` @ 0x97e64, 0x164바이트. 호출자 **4곳
전부**(**확인됨**):

| 주소 | 호출자 | 뜻 |
|---|---|---|
| 0x52eb0 | `Event_setQuestReward+0xb4` | 퀘스트 보상 |
| 0x52ee8 | `Event_setQuestReward+0xec` | 퀘스트 보상(펫?) |
| 0xca944 | `MONSTER_dropExp+0x78` | 몬스터 처치 |
| 0xca994 | `MONSTER_dropExp+0xc8` | 같은 값을 펫에게 |

본체:

```
if ((s8)HERO_getCurLevel(who) > 0x62) return 0;     ; 레벨 98 상한
hero[0x130] += amount                                ; 현재 경험치 (s32)
while (hero[0x130] >= HERO_getMaxExp(who)) { 레벨업 }
```

`MONSTER_dropExp`(0xca8cc)는 `heroLv - monLv > 5` 면 0, 아니면
`monLv<=0x28 ? monLv*8 : monLv * (monLv in 0x29..0x3b ? 12 : 19)`.

**진입에서 `r1` 을 곱하는 것으로 몬스터·펫·퀘스트가 전부 덮인다.** 레벨업 루프가
이미 다중 레벨업을 처리하므로 x4 도 안전하다. `hero[0x130]` 이 s32 이고
`HERO_getMaxExp` 를 넘으면 즉시 차감되므로 오버플로 여지도 없다.

### 2.7 장비 요구레벨 게이트는 정확히 7곳이다

아이템 구조체 **`item[7]` (s8) = 요구레벨**. 근거 두 가지(**확인됨**):

* `drawItemInfo+0xc6c`(0xf38d4): `sprintk(buf, getInCodeString(0x157), (s8)item[7])`
  이고 id 0x157 = `r요구레벨 &%d`.
* `ITEM_getData(hero, idx, field)`(0xf17c0)의 점프 테이블에서 **field 8** 이
  `ldrsb r0,[r3,#7]`.

게이트는 여섯 개의 거의 동일한 함수 + 인라인 복제 하나다.

```
Check_X(item):
  if (!item) return -1;
  reqLv  = (u8)item[7];
  heroLv = (u8)MP_getHero(CUR_HERO)[0x138];
  if ((s8)heroLv < (s8)reqLv) return -2;    ; 0xfe → "장착레벨이 맞지 않습니다"
  if ((s8)item[8] >= 0) return -1;
  kind = (u8)item[0x96]; ... 표 조회 → 슬롯 번호
```

| 함수 | 주소 | 아이템 인자 | `ldrb reqLv` | 워드 | `ldrb heroLv` |
|---|---|---|---|---|---|
| `Check_Hand` | 0xf1298 | r0 | +0x28 (0xf12c0) | `0xe5d56007` | +0x38 |
| `Check_LowerBody` | 0xf1324 | r0 | +0x28 (0xf134c) | `0xe5d56007` | +0x38 |
| `Check_UpperBody` | 0xf13b0 | r0 | +0x28 (0xf13d8) | `0xe5d56007` | +0x38 |
| `Check_Head` | 0xf143c | r0 | +0x28 (0xf1464) | `0xe5d56007` | +0x38 |
| `Check_LeftHand` | 0xf14c8 | **r1** | +0x2c (0xf14f4) | `0xe5d47007` | +0x3c |
| `Check_RightHand` | 0xf1584 | **r1** | +0x2c (0xf15b0) | `0xe5d58007` | +0x3c |
| 펫 장비 인라인 | — | r7 | `GameMenuFrame_KeyPressed+0xa28c` (0x820cc) | `0xe5d78007` | 0x820e0 |

`Check_*` 의 호출자는 각각 **2곳뿐**:
`checkEqipItem`(0xf16a8, 유일 호출자 `drawItemBag+0x30c`)과
`GameMenuFrame_KeyPressed+0xa2c8..0xa348`.

일곱째(펫)는 `item[0x96] == 0x17`(펫 장비) 가지에 손으로 복사된 같은 비교다.
0x820e0~0x820fc 에서 `heroLv >= reqLv` 면 `r8 = 0xff`, 아니면 `r8 = 0xfe`.

**`ITEM_getData` 를 후킹해서는 안 된다** — 전 바이너리에서 `ITEM_getData` 는
field 11~18, 33 으로만 불리고 **field 8 로는 한 번도 불리지 않는다**(전수 확인).

### 2.8 HP/SP HUD 의 위치, 그리고 메뉴가 그것을 다시 그리지 않는다는 사실

`Draw_PlayInfo(who, idx, mode)`(0x5aecc, 0xb4c)가 인게임 HUD다. 호출자는
`GameFrame_drawGameUI+0x30`(0x5c920)과 `+0x2d4`(0x5cbc4) **둘뿐**.
`idx` 는 파티 슬롯(0=주인공)이고 `r6 = idx * 0x23`(35) 이 세로 오프셋이다.
좌표(**확인됨**):

| 요소 | 호출 | 좌표 |
|---|---|---|
| 초상화 상자 | `DM_FillRectRGB` 0x5afc0 | x=2, y=idx\*35+3, 18x18 |
| 클립 | `DM_SetCliptoGBM` 0x5afd8 | (2, idx\*35+3, 0x16, idx\*35+0x17) |
| 레벨 숫자 | `HF_DrawTextSmallNumber` 0x5b0fc | y=idx\*35+0xf |
| HP 바 | `DM_FillRectRGB` 0x5b1b4 | x=0x19, y=idx\*35+8, w=`hp*0x2a/maxHp`, h=1 |
| HP 바 그림자 | `DM_FillRectRGB` 0x5b1e4 | x=0x19, y=idx\*35+9, h=2 |
| MP 바 | 같은 구조, `hero[0x128]/[0x12c]` | y=idx\*35+0x10 근처 |

즉 **HUD 블록은 대략 x ∈ [2, 67], y ∈ [3, 24]** 를 차지한다
(`hero[0x120]/[0x124]` = HP/최대HP, `hero[0x128]/[0x12c]` = MP/최대MP).
논리 화면 폭이 400이든 533이든 좌상단 절대좌표라 변하지 않는다.

**두 번째 호출(0x5cbc4)은 펫이다** — `Draw_PlayInfo(CUR_HERO + 1, 1, 0)`,
`hero[0x62c] != 0`(펫 보유) && `CHAR_getVisible` 일 때만. 즉 펫이 나와 있으면
`idx = 1` 줄, 즉 **y ∈ [38, 59]** 가 또 쓰인다. 버프 문구는 y ≤ 31 안에
머물러야 겹치지 않는다(3.6 의 3줄 배치가 그 조건을 만족한다).

**중요**: 인게임 메뉴(`_GameMenuState == 0xc`)의 렌더러 `IGM_DrawMenu`(0x6d184)는
HP/SP 를 그리지 않는다. 그 화면에 보이는 HUD 는 `GameMenuFrame_Initialize+0x288`
의 `DM_BoxBlurLCD565` 가 만든 **정지된 블러 스냅샷**이고
`GameMenuFrame_Render` 는 매 프레임 `DM_CopyOffScreen` 으로 그것을 되붙인다.
`IGM_DrawMenu` 가 그리는 두 개의 `HF_DrawTextSmallNumber`(0x6d480, 0x6d560)는
HP/MP 가 아니라 **`hero[0x170]` 스탯포인트 / `hero[0x16d]` 특성포인트 뱃지**다
(`mainFrame[0x157]` 로 게이트).

→ 설계상 의미: 메뉴에서 버프 문구를 그리려면 **`IGM_DrawMenu` 꼬리에서 우리가
직접 그려야** 하고, 배경은 블러된 HUD 다. 3.6 에서 두 배치안을 제시한다.

### 2.9 게스트 텍스트 렌더러와 EUC-KR

호스트에서 문자열을 그리는 방법은 이미 저장소에 있다 —
`patch.rs::host_cash_price` 가 그대로 한다:

```rust
let buf = self.cash_scratch()?;            // heap.alloc 으로 잡은 게스트 버퍼
self.mem.write_bytes(buf, &out);           // EUC-KR 바이트 그대로
self.call(draw, &[fb, x, y, buf, len, c1, c2, flags])?;   // HF_DrawTextOutLine
```

`HF_DrawTextOutLine(g, x, y, str, len, c1, c2, flags)` — 앞 4개는 레지스터,
뒤 4개는 스택(**저장소 코드에서 검증됨**). `flags`: 0=x 에서 시작,
1=중앙정렬, 2=우정렬. 폭은 `HF_GetWidth(s, len, font)`, `font = (c2 >= 0)`.

`|` `&` `$` `{` `}` `o`~`t` `;` 는 **폭 0 색상 이스케이프**다
(`HF_IsEscape` + `HF_getColorChar`). `;` 는 다중행 렌더러에서 줄바꿈이기도 하다.

**한글은 EUC-KR 바이트 배열 리터럴로 소스에 박는다.** `String` 을 거치면 안
된다는 CLAUDE.md 규칙 그대로다. 필요한 문자열의 바이트는 부록 C.

### 2.10 게스트에도 버프 시스템이 있지만 쓰지 않는다

`hero + 0x2ec + kind*0x10` 이 버프 레코드다:

* `HERO_Get_Buff(who, k)` → `hero[0x2f8 + k*0x10]` (u8 활성 플래그)
* `HERO_Get_Buff_Value(who, k)` → `hero[0x2ec + k*0x10]` (s32 값)
* `HERO_Set_Buff(who, effIdx, durUnits, value, ?, slot)` — 지속시간은
  `durUnits * 0x14` 틱, `hero[0x2f0 + k*0x10]`
* `HERO_BuffOff`, `HERO_Buff_Process`(0x95bac), `HERO_Reset_BUffAll`

**쓰지 않는 이유 세 가지**: (a) 슬롯 kind 는 스킬 버프 전용으로 이미 할당돼 있어
빈 칸을 고르는 것이 곧 회귀 위험이다, (b) 이 표는 `HERO_Reset_BUffAll` 로
사망·이벤트 전환마다 지워진다(0x573ec, 0x57440, 0x5ec2c, 0x5ec34, 0x905b0) —
"계정 지속" 요구와 정면으로 어긋난다, (c) 저장되지 않는다.
→ **상태는 호스트가 들고 있어야 한다.**

## 3. 설계

### 3.1 아이템 정체성 — 기존 id 재사용 + 새 subType

| 이름 | id | subType | 가격 | 수량 | 등급색 | 효과 | 지속 |
|---|---|---|---|---|---|---|---|
| 성장의 비약 | `0x23` | `0x10` | 1000 | 1 | 4 | 획득 경험치 x2 | 7일 |
| 슈퍼성장의 비약 | `0x23` | `0x11` | 2000 | 1 | 4 | 획득 경험치 x4 | 7일 |
| 종결자의 비약 | `0x23` | `0x12` | 1500 | 1 | 4 | 장비 요구레벨 -5 | 7일 |

`수량`(`_merchant + 0x11fc`)은 **1로 둔다.** 이 필드는 `dealCnt` 곱셈에 쓰여
`netSocket[0xb8]` 로 넘어가는데, 가방에 넣지 않는 우리 아이템에서는 의미가
없고 1이 아닌 값은 "5개 묶음"처럼 보여 혼란만 준다. 7일 누적 연장을 원하는
플레이어는 그냥 두 번 사면 된다.

`0x23` = 고대비약의 id. 근거:

* 2.3 — 상점 아이콘은 **id 로만** 고르므로 물약 아이콘이 공짜로 붙는다.
* `NpcUI_isCashTem(id)`(0xe19c8)은 `NPC/MERCHANT_2` 를 **자기가 다시 읽어서**
  id 만 비교한다. 0x23 은 이미 캐시 id 이므로 답이 바뀌지 않고, 그 함수가
  `cashTemType`(0x14바이트)에 쓰는 개수도 파일 그대로 12 라 넘치지 않는다.
* `NameTitle_checkCashItemNGet(id, subType)`(0xa5990)은 `_nameTitle+0xc64/0xc68`
  과 둘 다 같아야 발동하는 업적 카운터라 새 subType 은 조용히 지나간다.
* 우리 훅은 `(id == 0x23 && subType >= 0x10)` 으로 정확히 셋만 집는다 —
  진짜 고대비약(subType 0)은 손대지 않는다.

subType 0x10 이상을 쓰는 이유는 출고 파일의 subType 최대값이 2 이기 때문이다
(부록 B). 여유를 두면 나중에 넷째 비약을 추가해도 충돌하지 않는다.

> **대안**: 아예 새 id 를 쓰면 `getImage(_imgITMIcon, id)` 가 배열 밖을 읽어
> 쓰레기 아이콘이나 폴트가 난다. 새 아이콘을 넣으려면 `_imgITMIcon` 배열 자체를
> 늘려야 하고 그건 `ITEM_LoadEx` 경로 전체를 건드리는 일이다. **하지 말 것.**

### 3.2 상점 목록 주입 — `NpcUI_setMerchant` 진입 래핑

`Patch::CashPotionList`, 심볼 `NpcUI_setMerchant` 에 설치.

```
host_cash_potion_list():
    if LR == RETURN_MAGIC          -> decline        (재진입 방지)
    if !potions_enabled()          -> decline
    if r0 != 2                     -> decline
    if (s8)*merchantState != 3     -> decline        (캐시상점만)
    lr = LR
    self.call(NpcUI_setMerchant, &[2])               ; 게스트가 파일을 파싱
    n = mem.r16(_merchant)                           ; 12
    if n == 0 || n + 3 > 20        -> PC = lr; return served   (그냥 원본만)
    for (k, p) in POTIONS.enumerate():
        i = n + k
        mem.w8 (_merchant + 0x02   + i,          p.id)
        mem.w8 (_merchant + 0x66   + i,          p.sub)
        write_cstr(_merchant + 0xca   + i*0x28,  p.name_euckr)   // <= 0x27 + NUL
        mem.w32(_merchant + 0x106c + i*4,        p.price)
        mem.w8 (_merchant + 0x11fc + i,          1)              // qty
        write_cstr(_merchant + 0x1260 + i*0xff,  p.desc_euckr)   // <= 0xfe + NUL
        mem.w8 (_merchant + 0x75fc + i,          4)              // gradeColour
    mem.w16(_merchant, n + 3)
    mem.w8(NPCUI_BAG_MAX, ceil((n+3) / 20))          // 15 -> 1
    PC = lr; served
```

주의점:

* **`n + 3 > 20` 이면 주입하지 않는다.** 20 은 `cashTemType`(0x14바이트) 한계가
  아니라 — 그 함수는 파일을 따로 읽으니 12로 그대로다 — **한 페이지 20칸**과,
  15개를 한 페이지에 두겠다는 우리 결정이다. 21개 이상을 원하면
  `NPCUI_BAG_MAX` 만 올리면 되지만 페이지 넘김 UI 를 실기기에서 확인해야 한다.
* 종결자 0xff 는 `_merchant + 2 + 15 .. + 0x66` 에 그대로 남는다 —
  게스트가 12 기준으로 채운 것을 우리가 앞 3칸만 덮어쓰기 때문이다. 정확히 맞다.
* 매번 상점을 열 때마다 다시 주입되므로 마을 상점을 갔다 와도 유지된다.
* `write_cstr` 은 반드시 **NUL 을 같이 쓴다**. 2.1 의 출고 버그를 물려받지 않기
  위해서다.

### 3.3 구매 가로채기 — `NpcUI_setBoughtItemOnCASH` 진입

`Patch::CashPotionBuy`, 심볼 `NpcUI_setBoughtItemOnCASH` 에 설치.

```
host_cash_potion_buy():
    if !potions_enabled()          -> decline
    slot = mem.r32(netSocket + 0xb4)
    if slot < 0 || slot >= 100     -> decline
    id  = (s8)mem.r8(_merchant + 0x02 + slot)
    sub = (s8)mem.r8(_merchant + 0x66 + slot)
    let Some(p) = POTIONS.find(id, sub) else -> decline     ; 나머지 11종은 게스트가
    self.potions.grant(p.kind, self.wall_now_ms());         ; 남은 시간에 7일 누적
    self.potions.save(&self.cfg.storage);
    // 게스트 꼬리를 그대로 재현 — errIdx 6 = "구입하였습니다"
    mem.w32(dealCnt, 1);
    mem.w8(errIdx, 6);
    mem.w8(netSocket + 0x18, 0);
    mem.w8(dealSel, 0);
    mem.w8(npcUI_bDeal, 0);
    self.call(GameFrame_autoSave, &[])?;
    PC = LR; served
```

`NameTitle_checkCashItemNGet` 은 **부르지 않는다** — 그것은 "특정 캐시 아이템을
샀다"는 업적이고 우리 아이템은 그 대상이 아니다. (부르고 싶다면 인자가
`(id, subType)` 이라 그냥 통과한다. 어느 쪽이든 무해.)

`ItemBag_leftSlotCntOnAllBag() > 0` 게이트(2.4, `NpcUI_merchantSelectKeyPressed`
0xe1e3c)는 **우리 훅보다 앞에** 있으므로, 가방이 꽉 차면 비약도 못 산다.
가방에 들어가지도 않는 물건이라 이상하지만 **고치지 않기를 권한다** — 그 검사를
비활성화하려면 구매 키 핸들러를 또 하나 후킹해야 하고, 이득이 "가방 꽉 찬
상태에서도 살 수 있다" 뿐이다. 문서에만 남긴다.

`CS_FREE_CASH=0` 이면 `complete_mtx` 가 -1 로 답하고 `MTX_Result_CB` 가 아예
불리지 않는다 → **새 비약도 살 수 없다**(상점이 그 상태에서 못 빠져나온다).
즉 이 기능은 `CS_FREE_CASH` 에 종속이다. `potions_enabled()` 안에
`net::free_cash()` 를 AND 로 넣는 것이 정직하다.

### 3.4 효과 1 — 경험치 배수

`Patch::ExpMul`, 심볼 `HERO_add_Exp` 진입.

```
host_exp_mul():
    if LR == RETURN_MAGIC -> decline
    let m = self.potions.exp_mul(self.wall_now_ms());  ; 1 | 2 | 4
    if m == 1 -> decline                              ; 버프 없으면 원본 그대로
    let amt = cpu.r1 as i32;
    if amt <= 0 -> decline
    cpu.r1 = amt.saturating_mul(m) as u32;
    decline                                            ; 인자만 고치고 게스트 실행
```

**`decline` 이 핵심이다.** 레지스터만 바꾸고 서브하지 않으면 인터프리터가 그
자리에서 원본 함수를 그대로 실행한다 — `host_menu_arrow` 가 쓰는 것과 같은
수법이고, 레벨업 루프·업적·스킬 습득 같은 것을 하나도 재현할 필요가 없다.

`r1` 이 진입 시점에 살아 있다는 것은 확인됨: `HERO_add_Exp+0x8` 이
`mov r6, r1` 로 곧장 대피시킨다.

**성장/슈퍼성장은 같은 부류다.** 둘 다 걸려 있으면 배수는 4(최댓값), 만료는 각각
따로 온다 — 슈퍼가 먼저 끝나면 자동으로 2배로 내려앉는다. 같은 것을 또 사면
남은 시간에 **7일을 더한다**(누적 연장, 상한 없음). 상한을 두고 싶으면
`CS_POTION_DAYS` 의 배수로 자르면 되지만, 캐시템의 관례는 누적이다.

### 3.5 효과 2 — 장비 요구레벨 -5

7개 지점 전부를 덮어야 한다. 두 가지 방법이 있고 **A 를 권한다.**

**A. `Check_*` 6개는 진입 래핑 + 게스트 값 임시 하향 (권장)**

```
host_equip_level(which):
    if LR == RETURN_MAGIC -> decline
    if !self.potions.finisher(self.wall_now_ms()) -> decline
    let item = cpu.reg(which.item_reg());        ; Hand/Head/Upper/Lower=r0, L/RHand=r1
    if item == 0 -> decline
    let old = mem.r8(item + 7);
    let new = old.saturating_sub(5);             ; u8 산술, 0 클램프
    if new == old -> decline
    mem.w8(item + 7, new);
    let r = self.call(fn_addr, &[cpu.r0, cpu.r1])?;
    mem.w8(item + 7, old);                       ; 반드시 복원
    cpu.r0 = r; PC = LR; served
```

* 게스트 함수 전체(`item[8]` 검사, `item[0x96]` 부류 표 조회, 슬롯 번호 산출)를
  그대로 돌리므로 재현 위험이 0이다.
* 쓰기는 **동기적으로 되돌린다**. 단일 스레드이고 그 사이에 다른 게스트 코드가
  돌지 않으므로 관측 가능한 상태 변화가 없다.
* `item + 7` 은 아이템 인스턴스가 아니라 아이템 **템플릿**일 수 있다 —
  `ITEM_getItemFrom` 이 돌려주는 포인터의 수명은 확인하지 않았다.
  **복원하므로 어느 쪽이든 안전하다.** (미검증 사항으로 남긴다.)

**펫 인라인(0x820cc)은 단일 명령 패치**로 처리한다. `ldrb r8,[r7,#7]`
(`0xe5d78007`)을 후킹해 `r8 = max(0, item[7] - 5)` 를 넣고 `PC += 4`.
설치 시 명령어 워드를 확인하고 다르면 설치하지 않는다 —
`BAG_RIGHT_TEST_WORD` 와 같은 규약.

**B. 7곳 전부 단일 명령 패치** — 표 2.7 의 `ldrb` 자리를 전부 후킹. 코드가 더
짧지만 `Check_LeftHand`/`RightHand` 만 오프셋이 다르고 목적 레지스터도
r6/r7/r8 로 제각각이라 표를 손으로 관리해야 한다. A 보다 실수하기 쉽다.

**표시도 같이 내릴 것인지**는 별도 결정이다. `drawItemInfo+0xc6c`(0xf38d4)의
`ldrsb r2,[r4,#7]`(`0xe1d420d7`)를 후킹하면 툴팁의 "요구레벨 N" 이 내려간 값으로
보인다. **권장: 내린다.** 안 내리면 "요구레벨 45 인데 44에서 장착됨"이 버그처럼
보인다.

**만료 시 동작**: 게스트는 장착 중인 장비를 다시 검사하지 않는다
(`Item_loadStorage`/`HERO_load_data` 도 `ITEM_setEquip` 을 무검사로 부른다).
따라서 **버프가 끝나도 이미 장착한 장비는 그대로 남는다.** 이것을 버그로 볼지
사양으로 볼지는 제품 결정이다. 강제 해제를 원하면 `HERO_End_Process` 근처에
별도 훅이 필요하고, 그건 이 문서의 범위 밖이다. **사양으로 두기를 권한다** —
원본 게임의 다른 영구 캐시템(확장가방, 오토루팅)과 같은 결이다.

### 3.6 버프 HUD

`Patch::PotionHud`. 두 배치안:

**안 1 — 메뉴 화면 (요청 그대로).** `IGM_DrawMenu`(0x6d184) 진입 래핑:
게스트를 먼저 돌리고(`Vm::call`), 그 위에 문구를 그린다.

```
x = 0x48 (72)          ; HUD 우측 끝 67 + 여백 5
y = 4, 13, 22          ; 최대 3줄, 줄 간격 9 (HF_getHeightEng = 10)
flags = 0              ; 좌정렬
```

배경이 **블러된 스냅샷**(2.8)이므로 외곽선 폰트(`HF_DrawTextOutLine`)로 그려야
읽힌다. `c2 < 0` 이면 외곽선 폰트가 아니라 작은 폰트다 — `host_cash_price` 가
쓰는 `c1/c2/flags` 값을 그대로 흉내내는 것이 가장 빠르다.

**안 2 — 인게임 HUD 상시.** `Draw_PlayInfo`(0x5aecc) 진입 래핑, 게스트 뒤에
같은 좌표로 그린다. 이러면 메뉴를 열었을 때 **블러 스냅샷에 이미 문구가 찍혀
있다** — 즉 안 1 없이도 요청이 충족되고, 덤으로 전투 중에도 보인다.

안 2 의 게이트 두 개(둘 다 필수):

* **`r1 == 0`** — 인자는 `(who, idx, mode)` 이고 펫은 `(CUR_HERO+1, 1, 0)` 으로
  들어온다(2.8). 거르지 않으면 펫 줄에도 같은 문구가 겹쳐 그려진다.
* **호출자가 `GameFrame_drawGameUI+0x30`(0x5c920)인지.** `GameFrame_drawGameUI`
  자체는 `GameFrame_Render+0x4ac` 말고 **`MainMenuOptionKey+0x234`(0xaaf08)**
  에서도 불린다 — 메인메뉴 환경설정의 미리보기다. 거기까지 버프 문구가 뜨는
  것은 원치 않을 것이다. `LR` 로 거르면 된다(`host_cash_price` 와 같은 수법).

**권장: 안 2 를 기본으로 하고 안 1 을 옵션으로.** 안 2 는 훅이 하나뿐이고
"메뉴에서 보인다"를 자동으로 만족한다. 다만 전투 화면이 더 복잡해지므로
`CS_POTION_HUD = 0(끔) | 1(메뉴만) | 2(항상)` 로 고르게 한다.

문구(설계, 조정 가능). 지속이 일주일이라 남은 시간의 자릿수가 크게 변한다 —
**세 단계 서식**을 쓴다:

```
남은 >= 24시간   →  성장 x2 6일 23시간
남은 >=  1시간   →  성장 x2 5시간 12분
남은 <   1시간   →  성장 x2 47분
```

`일`/`시간` 은 **내림**한다(6일 23시간 뒤에 6일 22시간). 초 단위는 쓰지 않는다 —
일주일짜리 버프에서 매초 바뀌는 숫자는 잡음이고, HUD 를 매 프레임 다시 그리는
비용도 무의미해진다.

버프가 하나도 없으면 **아무것도 그리지 않는다**(빈 HUD 가 낫다).
`MC_knlSprintk(buf, "r성장 x2 &%d일 %d시간", d, h)` 로 만들면 게스트 서식기가
숫자를 채운다 — 호스트에서 숫자를 문자열로 만들 필요가 없고, `r`/`&` 색상
이스케이프도 그대로 쓸 수 있다. 바이트는 부록 C.

`CS_POTION_DAYS=0`(영구)이면 시간 없이 이름만 찍는다.

### 3.7 상태와 영속성

**게스트 세이브(`kickass`)는 건드리지 않는다.** 호스트 사이드카 한 개:

```
<Config::storage>/cs_potions.json
{ "v": 1, "exp2_until": 0, "exp4_until": 0, "finisher_until": 0 }
```

값은 **UNIX epoch 밀리초 기준 만료 시각**(i64). 0 = 미보유.

### 왜 `Vm::now_ms()` 가 아닌가

일주일은 한 세션이 아니다. `now_ms()` 는 **프로세스 시작부터**의 시각이고
(`Config::virtual_clock` 이면 가상 시계), 앱을 껐다 켜면 0으로 돌아간다.
"남은 밀리초"를 저장하는 방식이면 **앱을 꺼 둔 시간이 흐르지 않는다** — 7일짜리
버프가 실제로는 "7일치 플레이 시간"이 되어 사실상 영구가 된다.

그래서 시각원을 하나 더 둔다:

```rust
/// 벽시계 epoch ms. 버프 만료는 이것으로만 판정한다.
fn wall_now_ms(&self) -> i64 {
    if self.cfg.virtual_clock {
        // 프로브: 결정적 재생을 위해 고정 기점 + 가상 시계.
        // 같은 --frames/--ms 면 언제 돌려도 같은 값이 나온다.
        POTION_TEST_EPOCH + self.now_ms()
    } else {
        SystemTime::now().duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64).unwrap_or(0)
    }
}
```

`virtual_clock` 분기가 없으면 프로브 덤프가 실행 시각에 따라 달라져서
**"픽셀 단위 동일" 회귀 시험 자체가 불가능해진다** — HUD 가 남은 시간을 찍기
때문이다. 이 분기는 선택이 아니라 필수다.

### 나머지

* 기록 시점: 구매 직후(3.3), 그리고 종료 시(`cs_android` 의 `process::exit(0)`
  직전 — `files.rs` 가 `BufWriter` 없이 쓰는 것과 같은 이유로 안전하다).
  구매 시점에 이미 쓰므로 종료 시 쓰기는 사실상 보험이다(만료는 시각 비교라
  "닫는 시점"을 기록할 필요가 없다). 이것이 벽시계 방식의 부수 이득이다.
* 읽기 시점: `Vm::new` 에서 한 번. 파일이 없거나 깨졌으면 전부 0 으로 시작하고
  **경고 한 줄만 찍는다** — 여기서 죽으면 앱이 안 뜬다.
* "계정" 단위 = 설치 단위. 세이브 슬롯별로 나누고 싶다면 파일명에 슬롯을 붙여야
  하는데, 현재 로드된 슬롯을 호스트가 알 방법은 확인하지 않았다. **미해결**.
* **기기 시계를 바꾸면 버프가 늘거나 사라진다.** 싱글플레이이고 서버가 검증하지
  않으므로 막지 않기를 권한다. 굳이 막겠다면 "마지막으로 관측한 벽시계"를 같이
  저장해 **역행만** 감지하는 정도가 한계다(앞으로 돌리는 것은 못 막는다).

지속시간 기본값: **7일**(`CS_POTION_DAYS = 7`).
`CS_POTION_DAYS=0` 이면 **영구**(오토루팅과 같은 결) — 만료 비교를 건너뛴다.
시험용으로 분 단위가 필요하면 `CS_POTION_MINUTES` 가 있으면 편하다
(설정되면 `CS_POTION_DAYS` 를 무시). 만료 경로를 실기기에서 밟아 보려면 그
방법뿐이다 — 일주일을 기다릴 수는 없다.

### 3.8 환경 변수

CLAUDE.md 규약대로 **환경변수와 빌드시점 둘 다** 읽어야 한다
(`option_env!` + `std::env::var`, 기기에는 환경이 없다).
`CS_SERVER_HOST` 와 같은 구조다. (빌드시점 값을 바꾸면 `cargo clean -p cs_core`
가 필요했지만 `cs_core/build.rs` 가 그것을 닫았다 — 2026-08-16.)

| 변수 | 기본 | 뜻 |
|---|---|---|
| `CS_POTIONS` | 1 | 3종 전체 on/off |
| `CS_POTION_DAYS` | **7** | 지속 일수. 0 = 영구 |
| `CS_POTION_MINUTES` | (없음) | 설정되면 `CS_POTION_DAYS` 를 무시. 만료 시험용 |
| `CS_POTION_HUD` | 2 | 0 끔 / 1 메뉴만 / 2 항상 |
| `CS_POTION_EXP` | 1 | 경험치 배수 효과만 끄기 |
| `CS_POTION_LEVEL` | 1 | 요구레벨 -5 효과만 끄기 |
| `CS_POTION_LEVEL_DELTA` | 5 | 내릴 레벨 수 |

### 3.9 이미 있는 free-cash 패치와의 상호작용 (확인됨)

셋 다 새 항목에 **자동으로 적용되고, 그것이 바람직하다.**

| 기존 패치 | 훅 | 새 비약에 미치는 영향 |
|---|---|---|
| `host_cash_price` | `HF_DrawTextOutLine` @ `NpcUI_DrawMerChant_Info+0x664` | 선택된 항목이 무엇이든 가격에 **가로줄 + " 0"** 을 그린다. 새 비약도 "~~1000~~ 0" 으로 보인다 — 다른 11종과 동일. |
| `host_cash_skip_confirm` | `NpcUI_merchantSelectKeyPressed` (`LR == NpcUI_merchantKeyPressed+0x5e0` && `merchantState == 3`) | 1패스를 `Vm::call` 로 먼저 돌려 **한 번 누르면 구매**. 위에서 확인한 대로 새 항목의 1패스는 통과하므로 그대로 동작한다. |
| `host_fee_wording` | `getInCodeString` id `0x217..0x221` | 데이터 관리 전용. 무관. |

**따라서 3.3 의 훅은 `host_cash_skip_confirm` 보다 *뒤* 단계에 있다** —
1패스/2패스를 어떻게 접든 최종 지급은 `NpcUI_setBoughtItemOnCASH` 한 곳이다.
두 패치는 서로 모르는 채로 공존한다.

`CS_CASH_CONFIRM=1`(두 번 눌러 구매)로 돌려도 경로는 같다.

## 4. 구현 순서

의존 관계를 따라 자른 순서다. 각 단계 끝에 검증 항목을 붙였다 — 앞 단계가
초록이 아니면 다음으로 가지 말 것. §6 의 번호와 대응한다.

1. **`potions.rs` 골격 + 사이드카 + `wall_now_ms()`.** `PotionState`(만료 epoch
   ms 3개), `load/save`, `exp_mul()`, `finisher()`, `remaining()`, 그리고
   부록 C 의 바이트 상수. `Vm` 에 필드 추가. **`virtual_clock` 분기를 여기서
   같이 넣는다**(3.7) — 나중에 붙이면 그 전에 찍은 덤프가 전부 무효가 된다.
   아직 어떤 패치도 걸지 않는다. → 빌드가 통과하고 §6-1 이 초록.
2. **`CS_POTION_*` 노브.** `patch.rs::knob` / `strike_y()` 와 같은
   "환경변수 + `option_env!`" 이중 읽기. 전부 기본값이면 기능이 완전히 꺼져야
   한다. → §6-1 재확인.
3. **`Patch::ExpMul`.** 가장 작고 가장 독립적이다. 프로브 강제 부여 플래그
   (`--potion exp2`)를 같이 넣어 상점 없이 시험한다. → §6-4.
4. **`Patch::EquipLevel` 6종 + `EquipLevelPet` + `ItemInfoLevel`.** 여기서
   `Vm::call` 재진입 가드와 `item[7]` 복원을 확실히 한다. → §6-5, §6-6.
5. **`Patch::CashPotionList`.** 상점에 3칸이 뜨는지, 아이콘·이름·가격·설명이
   맞는지. 아직 구매는 게스트가 처리하므로 **사면 가방에 고대비약이 들어간다** —
   정상이다, 다음 단계에서 막는다. → §6-2.
6. **`Patch::CashPotionBuy`.** 이제 구매가 버프로 바뀐다. → §6-3.
7. **`Patch::PotionHud`.** 마지막. 앞의 전부가 초록이어야 화면으로 상태를
   확인할 수 있다. → §6-7.
8. **만료와 재시작.** `CS_POTION_MINUTES=1` 로 만료를 밟고, 앱을 껐다 켜서
   남은 시간이 줄어드는지 본다. → §6-8, §6-9.
9. **실기기 + 문서.** → §6-10, 그리고 CLAUDE.md/ANALYSIS.md 절 추가.

3~7 은 각각 독립적으로 `CS_POTION_*` 로 끌 수 있어야 한다. 실기기에서 한 조각만
문제가 생겼을 때 나머지를 살린 채로 이분하기 위해서다 — `CS_NO_FNT` 가 폰트
패치만 끄는 것과 같은 이유.

## 5. 파일별 변경 목록

| 파일 | 변경 |
|---|---|
| `cs_core/src/patch.rs` | `Patch` 에 `CashPotionList`, `CashPotionBuy`, `ExpMul`, `EquipLevel(Slot)`, `EquipLevelPet`, `ItemInfoLevel`, `PotionHud` 추가. `PATCHED_POTIONS` 테이블(심볼→패치). `host_*` 7개. 명령어 워드 검증 상수 2개(펫 인라인, drawItemInfo). |
| `cs_core/src/lib.rs` | 설치(`Vm::new` 의 free-cash 블록 옆), `run()` 디스패치 arm 추가, `Vm` 에 `potions: PotionState` 필드. |
| `cs_core/src/potions.rs` (신규) | `PotionState` — 3종 **만료 epoch ms**, `exp_mul()`, `finisher()`, `grant()`, `remaining()`, `load()/save()`, EUC-KR 문자열 상수, 상점 레코드 정의. |
| `cs_core/src/lib.rs` (추가) | `wall_now_ms()` — `virtual_clock` 이면 `POTION_TEST_EPOCH + now_ms()`, 아니면 `SystemTime::now()`. 3.7. |
| `cs_android/src/lib.rs` | 종료 직전 `potions.save()`(보험). |
| `cs_probe/src/main.rs` | `--potion exp2|exp4|finisher` 같은 강제 부여 플래그(회귀 테스트용, 상점까지 조작 안 하고 효과를 켜기 위해). |
| `CLAUDE.md` / `ANALYSIS.md` | 완료 후 절 추가. 2.1 의 descLen 정정도 같이. |

새 crate 의존성 **없음**. JSON 은 손으로 써도 되고(3줄), 정 필요하면 이미
`Cargo.lock` 에 있는 것만 쓴다.

## 6. 검증 계획

이 저장소의 규약은 "픽셀 단위 동일" 이다. 그대로 따른다.

기준 명령(CLAUDE.md 의 "부팅→슬롯→인게임 대사 2000프레임" 시나리오). **매 실행
storage 를 새로 줄 것** — 세이브가 실행 간에 이월되면 재생이 갈라진다:

```powershell
$T = "$env:TEMP\cs_ab"; Remove-Item -Recurse -Force $T -EA SilentlyContinue
cargo run --release -p cs_probe -- --frames 2000 --ms 33 --dump 2000 --profile
```

`--wipi 533x240` 로 한 번 더 돌려 두 구성 모두 확인한다.

1. **기능 off 회귀.** `CS_POTIONS=0` 으로 위 시나리오를 돌려 `CS_POTIONS` 도입
   **전 커밋**과 덤프 프레임이 바이트 단위 동일한지 확인. 패치가 전부
   decline 이어야 한다. 이 항목이 초록이 아니면 나머지는 볼 필요가 없다.
2. **상점 목록.** 프로브로 캐시상점을 열고 덤프 → 15칸, 새 3칸의 이름·가격이
   보이고 아이콘이 고대비약과 같은지. `_merchant` 를 `CS_WATCH` 없이 확인하려면
   `NpcUI_setMerchant` 직후 로그 한 줄.
3. **구매.** 새 아이템을 사고 (a) `errIdx` 가 6 이라 "구입하였습니다" 가 뜨는지,
   (b) **가방에 아무것도 안 들어갔는지**(`ItemBag` 슬롯 수 불변),
   (c) `cs_potions.json` 이 생겼는지, (d) `kickass` 가 구매 전후로 달라지는지
   — `GameFrame_autoSave` 를 부르므로 달라지는 것이 정상이다.
4. **경험치.** `CS_POTION_EXP=1` + 강제 부여로 몬스터 1마리 처치 시
   `hero[0x130]` 증가분이 정확히 2배/4배인지. 로그로 `HERO_add_Exp` 인자를 찍어
   비교(`CS_WATCH=HERO_add_Exp` 가 이미 그 일을 한다).
5. **요구레벨.** 요구레벨이 캐릭터 레벨보다 1~5 높은 장비를 준비해
   버프 on 에서 장착되고 off 에서 "장착레벨이 맞지 않습니다" 가 뜨는지.
   **7개 지점 전부**: 무기(우수/좌수), 머리, 상의, 하의, 장갑, 펫 장비.
   `checkEqipItem` 경로(가방 목록의 회색 처리)와 `GameMenuFrame_KeyPressed`
   경로(실제 장착)가 **일치**하는지 — 어긋나면 "보이는데 안 눌리는" 화면이 된다.
6. **레벨 하향 복원.** 5번 직후 `item[7]` 이 원래 값인지 메모리 덤프로 확인.
   이것이 A안 전체의 안전성 근거다.
7. **HUD.** 버프 0/1/2/3개 각각에서 덤프. 400x240 과 533x240 두 구성.
   버프 0개일 때 덤프가 `CS_POTIONS=0` 과 **바이트 단위 동일**해야 한다.
   서식 세 단계(일/시간, 시간/분, 분)를 전부 밟으려면 `cs_potions.json` 의
   만료 시각을 손으로 써서 프로브를 돌리는 것이 가장 빠르다.
8. **만료.** 일주일을 기다릴 수 없으므로 두 갈래로 나눈다.
   * `CS_POTION_MINUTES=1` 로 실제 만료를 밟는다 — 만료 후 경험치 배수가 1로
     돌아오고, `Check_*` 가 원래 레벨을 요구하고, HUD 줄이 사라지는지.
   * `cs_potions.json` 에 **이미 지난** 만료 시각을 넣고 부팅 — 로드 직후
     버프가 없는 상태여야 하고, 파일을 지우거나 깨뜨렸을 때 경고 한 줄만 찍고
     정상 부팅해야 한다.
9. **앱 재시작 지속.** 사고 나서 앱을 완전히 종료(**런처로 재실행**, `am
   force-stop` 은 CLAUDE.md 의 `exitApp` 함정을 가린다) → 남은 시간이 **줄어든
   채로** 유지되는지. 이것이 3.7 의 벽시계 결정이 옳았는지를 보는 유일한 시험이다.
   기기 시계를 하루 앞으로 돌려 남은 시간이 하루 줄어드는지도 같이 본다.
10. **실기기.** `.\build_android.ps1 -Release -Install` 후 위 3·4·5·7·8·9 재현.
    `adb logcat -s chronos` 로 `perf:` 프레임 시간이 변하지 않는지도 같이 본다
    (패치가 매 프레임 도는 것은 HUD 하나뿐이라 변화가 없어야 한다).

## 7. 함정

* **`patch_lo..patch_hi` 이진 탐색.** 패치를 7개 더하면 창이 넓어진다. 지금
  `.text` 의 93% 를 덮고 있으므로 실질 영향은 없지만, `Vm::new` 끝의
  `sort_unstable_by_key` 뒤에 넣는 것을 잊으면 디스패치가 조용히 안 걸린다.
* **재진입.** `Vm::call` 은 `LR = RETURN_MAGIC` 로 들어가므로 진입 래핑형 패치는
  **반드시 첫 줄에서 `lr == RETURN_MAGIC` 이면 decline** 해야 한다. 이걸 빼면
  무한 재귀로 스택이 죽는다. `host_cash_price` 의 주석이 같은 얘기를 한다.
* **EUC-KR.** 한글은 `b"\xbc\xba..."` 리터럴로만. `String`/`format!` 를 태우면
  0x80 이상이 U+FFFD 가 되고 화면에 "썍몌예ㅃ" 가 나온다.
* **`now_ms()` 로 만료를 재면 안 된다.** 프로세스 상대 시각이라 앱을 꺼 둔
  일주일이 흐르지 않는다 — 7일 버프가 사실상 영구가 된다. 만료는 `wall_now_ms()`
  로만(3.7). 반대로 **HUD 가 벽시계를 직접 읽어도 안 된다** — 프로브 덤프가
  실행 시각마다 달라져서 회귀 시험이 불가능해진다. `virtual_clock` 분기는 필수.
* **`SystemTime::now()` 는 실패할 수 있다.** 기기 시계가 1970 이전이면
  `duration_since(UNIX_EPOCH)` 가 `Err` 다. `unwrap()` 금지 — 0 으로 떨어뜨리면
  모든 버프가 만료된 것으로 보이므로, 그 경우는 **만료 판정을 건너뛰는** 쪽이
  덜 나쁘다(플레이어가 산 것을 잃지 않는다).
* ~~**`cargo clean -p cs_core`.**~~ 닫혔다(2026-08-16) — `cs_core/build.rs` 가
  `option_env!` 이름 전부를 `rerun-if-env-changed` 로 내보내므로 빌드시점 값을
  바꾸면 cargo 가 알아서 다시 컴파일한다.
* **`CS_NO_PATCH=1` 은 이 기능도 끈다.** 회귀 A/B 의 기준선이 되지만, "패치를 다
  꺼도 상점에 3종이 보인다"면 뭔가 잘못된 것이다.
* **`GameFrame_autoSave` 를 `Vm::call` 로 부르는 것**은 파일 I/O 를 동반한다.
  `hle.rs` 의 파일 경로가 그 시점에 유효한지(스토리지 루트 초기화 완료) 확인할
  것. 구매 시점이면 당연히 유효하지만, 부팅 중 훅에서는 아니다.
* **`descLen`.** 2.1 의 정정을 CLAUDE.md 에 반영하지 않으면 다음 사람이 또
  0x27 로 잘라 쓴다.
* **펫 인라인 사이트(0x820cc)** 는 `GameMenuFrame_KeyPressed` 안이라 함수 크기가
  0xa730 이다. 오프셋 `+0xa28c` 를 심볼 기준 상대값으로 계산하고 워드
  `0xe5d78007` 를 설치 시 검증할 것.
* **capstone 이 함수 중간 데이터에서 멈춘다.** 부록 D 의 덤프 스크립트에는
  재개 로직이 들어 있다 — 없으면 `GameMenuFrame_KeyPressed` 의 뒷부분
  약 680 KB 가 통째로 안 보이고, 이 분석의 2.7절 인라인 사이트를 놓친다.
  (실제로 처음에 놓쳤다.)

## 8. 미해결 / 결정 필요

~~1. **지속시간.**~~ **확정: 셋 다 7일**(벽시계). 3.7.

1. **세이브 슬롯별 vs 계정 단위.** 현재 로드된 슬롯을 호스트가 아는 방법을
   확인하지 않았다. 계정 단위(파일 1개)로 시작하기를 권한다 — 캐시로 산
   일주일짜리 버프라면 그쪽이 자연스럽기도 하다.
2. **버프 만료 시 초과 레벨 장비 강제 해제 여부.** 3.5. 하지 않기를 권한다.
   일주일이 지나 만료됐을 때 갑자기 장비가 벗겨지는 편이 더 나쁘다.
3. **성장/슈퍼성장 중첩 규칙.** 최댓값 채택 + 각자 만료(설계안). 재구매는
   7일 누적.
4. **`ItemBag_leftSlotCntOnAllBag` 게이트.** 가방이 꽉 차면 비약도 못 산다.
   그대로 두기를 권한다.
5. **`item[7]` 이 템플릿인지 인스턴스인지.** 3.5 A안은 복원하므로 무관하지만,
   B안(단일 명령 패치)을 고르면 알 필요가 없어지고, 다른 용도로 이 필드를 쓰려면
   먼저 확인해야 한다.
6. **HUD 문구 확정.** 3.6 의 세 단계 서식은 초안이다.
7. **기기 시계 조작 대응.** 막지 않기를 권한다(3.7). 막겠다면 역행 감지까지가
   한계다.

---

## 부록 A — 주소 요약

### 훅 지점

| 심볼 | 주소 | 크기 | 진입 워드 |
|---|---|---|---|
| `NpcUI_setMerchant` | 0x000e20a4 | 0x2e8 | `0xe92d4ff0` |
| `NpcUI_setBoughtItemOnCASH` | 0x000d6654 | 0x0b4 | `0xe92d4070` |
| `HERO_add_Exp` | 0x00097e64 | 0x164 | `0xe92d47f0` |
| `Check_Hand` | 0x000f1298 | 0x08c | — |
| `Check_LowerBody` | 0x000f1324 | 0x08c | — |
| `Check_UpperBody` | 0x000f13b0 | 0x08c | — |
| `Check_Head` | 0x000f143c | 0x08c | — |
| `Check_LeftHand` | 0x000f14c8 | 0x0bc | — |
| `Check_RightHand` | 0x000f1584 | 0x124 | — |
| 펫 인라인 | 0x000820cc | — | `0xe5d78007` |
| `drawItemInfo` 요구레벨 표시 | 0x000f38e8 | — | `0xe1d420d7` |
| `Draw_PlayInfo` | 0x0005aecc | 0xb4c | `0xe92d4ff0` |
| `IGM_DrawMenu` | 0x0006d184 | — | `0xe92d4ff0` |

### 읽는 전역

| 심볼 | 주소 | 크기 |
|---|---|---|
| `_merchant` | 0x00173390 | 0x7660 |
| `merchantState` | 0x0012abd0 | 1 |
| `NPCUI_BAG_MAX` | 0x0017a9f3 | 1 |
| `cashTemType` | 0x0017a9f8 | 0x14 |
| `npcUI_bDeal` | 0x0012abc0 | 1 |
| `dealSel` | 0x00173382 | 1 |
| `dealCnt` | 0x00173374 | 4 |
| `errIdx` | 0x00131fed | 1 |
| `errPop` | 0x00131fec | 1 |
| `_BagSlotIdx` | 0x0017337f | 1 |
| `bagSelCnt` (페이지) | 0x0017aa0c | 1 |
| `netSocket` | GOT[0x3c4] | `+0xb4` 슬롯, `+0xb8` 수량, `+0x17/+0x18` 플래그 |
| `_imgITMIcon` | GOT[0xbbc] | 아이콘 배열 포인터 |
| `_lcdG` | GOT[0x934] | `+4` graphics, `+0x4c` W, `+0x50` H |
| `CUR_HERO` | GOT[0x3d8] | 1 |
| `_GameMenuState` | — | `0xc` = 3x3 아이콘 메뉴 |

### hero 구조체 (`MP_getHero(idx)` 반환)

| 오프셋 | 뜻 |
|---|---|
| 0x120 / 0x124 | 현재 HP / 최대 HP |
| 0x128 / 0x12c | 현재 MP / 최대 MP |
| 0x130 | 현재 경험치 (s32) |
| 0x138 | 레벨 (u8, 상한 0x62) |
| 0x16d | 특성 포인트 (u8) |
| 0x170 | 스탯 포인트 (s16) |
| 0x2ec + k\*0x10 | 버프 값 (s32) |
| 0x2f0 + k\*0x10 | 버프 총 지속 (u16) |
| 0x2f2 + k\*0x10 | 버프 경과 (u16) |
| 0x2f8 + k\*0x10 | 버프 활성 (u8) |

### 아이템 구조체 (`ITEM_getItemFrom` 반환)

| 오프셋 | 뜻 | `ITEM_getData` field |
|---|---|---|
| +1 | (s8) | 3 |
| +2 | (s16) | 4 |
| +7 | **요구레벨 (s8)** | 8 |
| +8 | (s8) 0 이상이면 장착 불가 | 9 |
| +0x10 | (u16) | 2 |
| +0x96 | 부류 (u8) — 0x10..0x12 손, 7..9 머리, 0x16/0x17 펫 | — |

### 문자열

`getInCodeString(id)`(0xee888)는 **0x112e0c** 에서 0xc1c 바이트(=775 엔트리)를
스택으로 복사한 뒤 `table[id]` 를 돌려준다. 전체 표는 부록 D 의 `incode.py` 로
언제든 다시 뽑을 수 있다. 이 문서에서 쓴 것:

| id | 내용 |
|---|---|
| 0 | 슬롯이 부족합니다. |
| 1 | 소지금이 부족합니다. |
| 6 | 구입하였습니다 |
| 0x8e (142) | 이미 오토루팅이 적용되어있습니다. |
| 0xc8 (200) | 장착레벨이 맞지 않습니다. |
| 0xc9 (201) | 장착 할 수 없는 아이템입니다. |
| 0x157 (343) | `r요구레벨 &%d` |
| 0x2ea (746) | `\|CASH &%d` |

`NpcUI_DrawErrorMsg`(0xd759c) = `getInCodeString(errIdx)` (오프셋 0).
`IGM_drawErrMsg`(0x64484) = `getInCodeString(errIdx + 0x88)`.

## 부록 B — 출고 `NPC/MERCHANT_2` 전문

djb2 해시 `0x4f654898`, 768바이트, 12개, 768/768 완전 소비.

| # | id | sub | 가격 | 수량 | 등급 | 이름 | 설명 |
|---|---|---|---|---|---|---|---|
| 0 | 0x2a | 0 | 1000 | 5 | 4 | 제련블록제거X5 | 제련블록된 장비를 제련가능하게 풀어줍니다 |
| 1 | 0x23 | 0 | 100 | 1 | 4 | 고대비약 | 사용 즉시 tHP 2104~2420 MP 1696~1950& 회복 |
| 2 | 0x23 | 0 | 500 | 5 | 4 | 고대비약X5 | (위와 같음) |
| 3 | 0x13 | 0 | 1500 | 1 | 4 | 오토루팅 | 해당 슬롯에서 드랍된 아이템이 영구적으로 자동 획득됩니다 |
| 4 | 0x3e | 0 | 1000 | 1 | 4 | 확장가방 | 인벤토리를 영구적으로 확장합니다 |
| 5 | 0x14 | 0 | 1000 | 1 | 4 | 스탯초기화물약 | 캐릭터의 스탯을 초기화합니다 |
| 6 | 0x15 | 0 | 1000 | 1 | 4 | 특성초기화물약 | 캐릭터의 특성을 초기화합니다 |
| 7 | 0x28 | 0 | 1000 | 1 | 4 | 하급 유물함 | \|80레벨 후반& 대 유용한 아이템을 랜덤하게 획득합니다 |
| 8 | 0x28 | 1 | 2000 | 1 | 4 | 중급 유물함 | \|90레벨 후반& … |
| 9 | 0x28 | 2 | 3000 | 1 | 4 | 상급 유물함 | 싱글 네트워크 모두 유용한 \|최고 레벨 아이템&을 … |
| 10 | 0x35 | 0 | 2000 | 1 | 4 | 기본패키지 | 확장가방;오토루팅;고대비약X10 |
| 11 | 0x35 | 2 | 1500 | 1 | 4 | 초기화패키지 | 스탯 초기화;특성 초기화 |

설명 길이 최대 66바이트(#9) — 2.1 의 정정 근거.

## 부록 C — EUC-KR 바이트 리터럴

그대로 `cs_core/src/potions.rs` 에 붙여 쓸 수 있다. 길이는 NUL 제외.

```rust
// 상점 이름 (<= 0x27)
const NAME_EXP2:  &[u8] = b"\xbc\xba\xc0\xe5\xc0\xc7 \xba\xf1\xbe\xe0";                 // 성장의 비약        (11)
const NAME_EXP4:  &[u8] = b"\xbd\xb4\xc6\xdb\xbc\xba\xc0\xe5\xc0\xc7 \xba\xf1\xbe\xe0"; // 슈퍼성장의 비약     (15)
const NAME_FIN:   &[u8] = b"\xc1\xbe\xb0\xe1\xc0\xda\xc0\xc7 \xba\xf1\xbe\xe0";         // 종결자의 비약       (13)

// 상점 설명 (<= 0xfe, NUL 은 우리가 쓴다)
// 일주일 동안 획득 경험치가 2배가 됩니다  (38)
const DESC_EXP2: &[u8] = b"\xc0\xcf\xc1\xd6\xc0\xcf \xb5\xbf\xbe\xc8 \xc8\xb9\xb5\xe6 \xb0\xe6\xc7\xe8\xc4\xa1\xb0\xa1 2\xb9\xe8\xb0\xa1 \xb5\xcb\xb4\xcf\xb4\xd9";
// 일주일 동안 획득 경험치가 4배가 됩니다  (38)
const DESC_EXP4: &[u8] = b"\xc0\xcf\xc1\xd6\xc0\xcf \xb5\xbf\xbe\xc8 \xc8\xb9\xb5\xe6 \xb0\xe6\xc7\xe8\xc4\xa1\xb0\xa1 4\xb9\xe8\xb0\xa1 \xb5\xcb\xb4\xcf\xb4\xd9";
// 일주일 동안 장비 요구레벨이 5 낮아집니다 (40)
const DESC_FIN:  &[u8] = b"\xc0\xcf\xc1\xd6\xc0\xcf \xb5\xbf\xbe\xc8 \xc0\xe5\xba\xf1 \xbf\xe4\xb1\xb8\xb7\xb9\xba\xa7\xc0\xcc 5 \xb3\xb7\xbe\xc6\xc1\xfd\xb4\xcf\xb4\xd9";

// HUD 서식 — MC_knlSprintk(buf, fmt, a, b). 3.6 의 세 단계.
// 남은 >= 24시간:  MC_knlSprintk(buf, HUD_*_DH, 일, 시간)
const HUD_EXP2_DH: &[u8] = b"r\xbc\xba\xc0\xe5 x2 &%d\xc0\xcf %d\xbd\xc3\xb0\xa3"; // r성장 x2 &%d일 %d시간  (21)
const HUD_EXP4_DH: &[u8] = b"r\xbc\xba\xc0\xe5 x4 &%d\xc0\xcf %d\xbd\xc3\xb0\xa3"; // r성장 x4 &%d일 %d시간  (21)
const HUD_FIN_DH:  &[u8] = b"r\xc1\xbe\xb0\xe1\xc0\xda &%d\xc0\xcf %d\xbd\xc3\xb0\xa3"; // r종결자 &%d일 %d시간 (20)
// 남은 >= 1시간:  (시간, 분)
const HUD_EXP2_HM: &[u8] = b"r\xbc\xba\xc0\xe5 x2 &%d\xbd\xc3\xb0\xa3 %d\xba\xd0"; // r성장 x2 &%d시간 %d분  (21)
const HUD_EXP4_HM: &[u8] = b"r\xbc\xba\xc0\xe5 x4 &%d\xbd\xc3\xb0\xa3 %d\xba\xd0"; // r성장 x4 &%d시간 %d분  (21)
const HUD_FIN_HM:  &[u8] = b"r\xc1\xbe\xb0\xe1\xc0\xda &%d\xbd\xc3\xb0\xa3 %d\xba\xd0"; // r종결자 &%d시간 %d분 (20)
// 남은 < 1시간:  (분)
const HUD_EXP2_M:  &[u8] = b"r\xbc\xba\xc0\xe5 x2 &%d\xba\xd0";      // r성장 x2 &%d분  (14)
const HUD_EXP4_M:  &[u8] = b"r\xbc\xba\xc0\xe5 x4 &%d\xba\xd0";      // r성장 x4 &%d분  (14)
const HUD_FIN_M:   &[u8] = b"r\xc1\xbe\xb0\xe1\xc0\xda &%d\xba\xd0"; // r종결자 &%d분   (13)

// 영구 모드용 (CS_POTION_DAYS=0, 시간 없음)
const HUD_EXP2_P: &[u8] = b"r\xbc\xba\xc0\xe5 x2";          // r성장 x2   (7)
const HUD_EXP4_P: &[u8] = b"r\xbc\xba\xc0\xe5 x4";          // r성장 x4   (7)
const HUD_FIN_P:  &[u8] = b"r\xc1\xbe\xb0\xe1\xc0\xda";     // r종결자    (6)
```

가장 긴 HUD 줄은 `성장 x2 6일 23시간` 이다. 게임 폰트에서 한글 1글자 ≈ 12px,
ASCII ≈ 6px 이므로 대략 105px — HUD 우측 x=72 에서 시작하면 x≈177 에서 끝나
400 폭에서도 여유가 있다. 확정치가 필요하면 `HF_GetWidth` 를 호스트에서 불러
재면 된다(`host_cash_price` 가 이미 그렇게 한다).

선두 `r` 과 중간 `&` 는 게임 폰트의 **폭 0 색상 이스케이프**다 — 게임 자신의
문자열이 쓰는 관례를 그대로 따랐다(부록 A 의 id 0x157 참고).

## 부록 D — 이 분석에 쓴 도구

`tools_re/` 에 이미 `csdis.py`(디스어셈블), `csdis_got.py`(GOT 전역 주석),
`xref.py`(임포트 xref), `symtab.py` 가 있다. 여기에 더해 아래 넷을 임시로 만들어
썼다. 다음 세션이 다시 필요할 것이므로 요지를 남긴다 —
**저장소에는 커밋하지 않았다**(이 문서만 만들라는 요청이었다).

1. **`dumpall.py`** — `.text` 전 함수를 GOT/문자열 주석과 함께 한 파일(10 MB)로
   덤프. `csdis_got.py` 를 전 함수로 돌린 것 + **capstone 이 함수 중간
   데이터 워드에서 멈추면 4바이트 건너뛰고 재개**하는 루프. 이 재개가 없으면
   `GameMenuFrame_KeyPressed`(0xa730바이트) 뒷부분이 통째로 사라진다.
2. **`xref2.py`** — 내부 `bl/b` 호출자 xref(상류 `xref.py` 는 PLT 임포트만 본다).
3. **`incode.py` / `incode2.py`** — `getInCodeString` 표 775개 전문 덤프와,
   호출 지점의 상수 id 역추적(`mov r0,#a` + `add r0,r0,#b` 조합까지).
4. **`vfs.py` / `parsem.py`** — djb2 로 `data.vfs.mp3` 레코드를 뽑고
   `NPC/MERCHANT_*` 를 파싱.

공통 주의: Windows 에서 리다이렉트하면 한글 출력이 cp949 인코딩 오류로 죽는다.
`PYTHONIOENCODING=utf-8` 을 붙일 것.

```powershell
$env:PYTHONIOENCODING="utf-8"
python tools_re/csdis_got.py payload/libchronosMian.so NpcUI_setMerchant
python tools_re/armdiff.py --n 8000 --class all      # CPU 회귀 (기존)
```

**DWARF 에 대한 메모**: 이 `.so` 에는 `.debug_info` 가 26 CU 있지만, 전부
`midas*.cpp`(WIPI 플랫폼 계층)와 libgcc 다. **게임 본체는 디버그 정보 없이
컴파일됐다** — 구조체 이름을 기대하고 파고들 필요 없다. 다만 `.symtab` 이
살아 있어 함수 2734개 / 전역 이름은 그대로 쓸 수 있다.

---

## 9. 구현 결과 (2026-08-13) — 설계와 달라진 곳

전부 들어갔다. 새 crate 의존성 없음, 게스트 바이너리·VFS 무수정, 사이드카 한 개.
아래는 **이 문서가 틀렸거나 구현이 다른 길을 간 곳**만이다. 맞았던 곳은 반복하지
않는다.

### 9.1 요구레벨 게이트는 7곳이 아니라 9곳이다

2.7 절은 `Check_*` 여섯 개와 펫 인라인 하나를 세었다. `.text` 전체에서 **오프셋
7 의 바이트 로드를 전수 스캔**하면(`ldrb Rd,[Rn,#7]` 37개, `ldrsb` 39개) 하나가
더 나온다:

* **`GameMenuFrame_KeyPressed+0x2de0`(0x7ac20)** — `ItemBagKey` 가 0x65 를 돌려준
  뒤의 장착 경로. `ITEM_getItemFrom` → `ldrb r8,[r6,#7]` → `MP_getHero(1)` 의
  `[0x138]` 과 비교 → `item[0x96] == 0x1a` → `ITEM_setEquip(1, slot, item)`.
  펫 인라인(0xa28c, 클래스 0x17)과는 다른 블록이다.

여기에 표시용 `drawItemInfo` 를 더해 **9지점**. 심볼을 따라가면 못 찾고 명령어를
훑어야 나온다 — 6절의 검증 계획이 "7개 지점 전부"라고 쓴 것도 그래서 부족하다.

부록 A 의 `drawItemInfo` 주소(0xf38e8)가 맞고 2.7 의 `+0xc6c` 는 블록 머리다.
심볼 기준 오프셋은 **`+0xc78`**.

### 9.2 A안이 아니라 B안 — 한 명령만 대신 실행한다

3.5 는 A안(진입 래핑 + `item[7]` 임시 하향 + 복원)을 권했다. 구현은 **B안**이다.
B안의 약점으로 지적된 "표를 손으로 관리해야 한다"가 사라지기 때문이다 —
`Rd`/`Rn`/부호는 **명령어 워드에서 디코드**하면 되고, 그 워드는 어차피 설치 시
검증한다. 표에 있는 것은 `(심볼, 오프셋, 기대 워드)` 뿐이다.

그래서 얻은 것:

* **아이템 메모리에 아무것도 쓰지 않는다.** 8절 미해결 #5(`item[7]` 이 템플릿이냐
  인스턴스냐)가 답할 필요 없이 사라진다. 세이브에는 원래 요구레벨이 남는다.
* `Vm::call` 재진입도, 복원 실패 경로도 없다.
* 각 `Check_*` 안에 오프셋 7 로드가 **정확히 하나씩**임을 위 스캔이 증명하므로,
  한 명령을 고치는 것과 함수 전체에 낮춘 값을 보이는 것이 같다.

`0xfe`(= -2, "요구 없음")를 5 내리면 안 되므로 값은 **부호 있는 바이트로 읽어**
양수일 때만 내리고 0 에서 자른다. 되돌릴 때는 원래 명령의 부호 확장 여부를 그대로
따른다(`ldrb` 8곳은 0..255, `ldrsb` 한 곳은 부호 확장).

### 9.3 `NpcUI_setBoughtItemOnCASH` 는 호출자가 둘이다

1절 표는 "단일 진입점, 호출자 1곳"이라고 했다. 실제로는

* `MTX_Result_CB+0xac` (0xa5c50) — JNI 결제, 이 빌드가 실제로 쓰는 길
* `netLibCB+0x190` (0xd2a2c) — 소켓 충전 응답 경로

**진입 훅이므로 둘 다 덮인다.** LR 로 거르는 설계였다면 절반을 놓쳤을 것이다.

### 9.4 HUD 는 안1·안2 중 택일이 아니다 — 안1이 필수다

3.6 은 "안2(인게임 상시)면 메뉴의 블러 스냅샷에 문구가 이미 찍혀 있으므로 안1 없이
요청이 충족된다"고 했다. **찍혀 있기는 한데 같이 블러된다.** 반지름 5 로 뭉갠 한글은
읽을 수 없다. 그래서 `IGM_DrawMenu` 훅이 있어야 메뉴에서 읽힌다.

그리고 안2의 기본값도 뒤집었다. 인게임 프레임을 실제로 떠 보면
`GameFrame_drawGameUI` 가 **WORLD / SHOP / MAP / SAVE 를 x 95..305, y 0..12** 에
그리는데, 첫 버프 줄(x 72..177, y 2..14)이 그 위에 얹힌다. 그래서
**`CS_POTION_HUD` 기본은 1(메뉴만)**, 2 는 원하는 사람만.

훅 대상도 `Draw_PlayInfo` 가 아니라 **`GameFrame_drawGameUI`** 다 — 화면 전체가
끝난 뒤에 그려야 나중 그리기에 덮이지 않는다. 각각 호출자 하나로 고정한다
(`GameFrame_Render+0x4b0`, `GameMenuFrame_Render+0x648`). `MainMenuOptionKey+0x238`
(환경설정 미리보기)이 제외되는 것은 설계 그대로다.

줄 간격은 9 가 아니라 **14**다. 9 와 12 에서는 아랫줄 외곽선이 윗줄 한글의 마지막
획을 지워 `시간` 이 `시가` 로 나온다(덤프로 확인). 한글은 12행 + 외곽선 위아래 1행.

`MC_knlSprintk` 는 쓰지 않는다. 숫자는 ASCII 라 호스트에서 바이트로 이어 붙이면
되고, 그러면 매 프레임 게스트 호출이 하나 줄어든다. 색상 이스케이프(`r`, `&`)는
설계대로 게임 관례를 따랐다.

### 9.5 `wall_now_ms()` 에 `POTION_TEST_EPOCH` 는 필요 없다

3.7 의 스케치는 가상 시계에 상수 기점을 더했다. `Vm::clock_ms` 가 이미
`1_700_000_000_000` 에서 시작하므로 **가상 시계 값을 그대로 쓰면 된다** — 결정적이고
epoch 모양도 이미 맞다.

실패 처리는 `Option<i64>` 로 모델링했다. `None`(1970 이전 기기 시계)이면 만료
판정을 건너뛴다 — 산 것을 잃는 쪽이 더 나쁘다는 7절의 결론 그대로.

### 9.6 검증 결과

| 항목 | 결과 |
|---|---|
| §6-1 기능 off 회귀 | `CS_POTIONS=0` vs 기본, 400x240·533x240 두 구성 덤프 **픽셀 단위 동일**, 게스트 명령수도 동일 |
| §6-2 상점 목록 | count `0f`(15), id `23 23 23` + 종결자 0xff 유지, 이름/설명/가격/등급 전부 기대값, `NPCUI_BAG_MAX=1`. 화면에 15칸, 선택하면 "성장의 비약 / CASH ~~1000~~ / 일주일 동안 획득 경험치가 2배가 됩니다" |
| §6-3 구매 | `potions: exp2 granted` + `cs_potions.json` 생성, **가방 배열 바이트 단위 불변**(`_storage` 뒤 두 배열 A/B) |
| §6-4 경험치 | 실바이너리를 로드한 단위 테스트: x2/x4, 최댓값 채택, 0·음수 무시, 만료 후 원복 |
| §6-5 요구레벨 | 같은 방식으로 **9지점 전부** — 45→40, 3→0 클램프, 0xfe 는 부호 유지, 널 아이템은 인터프리터로 |
| §6-6 복원 | B안이라 쓰기 자체가 없음 |
| §6-7 HUD | 메뉴에서 3줄 렌더 확인(덤프), 버프 0개면 §6-1 과 동일 |
| §6-8 만료 | `active()` 단위 테스트(경계 ±1ms, 영구, 읽을 수 없는 시계) |
| §6-9 재시작 | 구매한 storage 로 새 프로세스 → `potions: exp2 7d 0h left` |
| §6-10 실기기 | **미실시** |

남은 것은 실기기 확인과, 인게임에서 요구레벨이 높은 장비를 실제로 장착해 보는
것뿐이다(= 위 9지점 테스트가 대신하고 있는 부분).

프로브를 인게임까지 몰고 가는 탭 시나리오는 `CLAUDE.md` 의 해당 절에 적어 두었다.

---

## 10. 요구레벨 표시 — 원래 값에 가로선, 뒤에 낮아진 값 (2026-08-13)

§9.2 는 아홉 지점을 전부 같은 방식으로 낮췄다. 그러면 툴팁이 `요구레벨 40` 이
되는데, 그건 **버프가 걸렸다는 사실을 지워 버린다** — 아이템이 원래 그런 것처럼
보인다. 그래서 표시만 따로 뗐다.

* `drawItemInfo+0xc78` 의 `ldrsb` 는 **패치하지 않는다.** 게스트가 원래 값을 찍게 둔다.
* `HF_DrawTextOutlineH2` 진입을 `LR == drawItemInfo+0xce0` 로 집어서, 문자열을
  `요구레벨 45 40` 으로 다시 만들어 게스트 렌더러로 그린 뒤 `45` 에 가로선을 긋는다.

즉 강제하는 지점은 여덟, 표시는 그리기 훅 하나다.

### 두 가지가 실측으로만 잡혔다

**1. `요구레벨-1`.** 시작 장비는 `item[7] == -1`(제한 없음)이라 게스트가
`요구레벨-1` 을 찍는다. 첫 **숫자**를 찾는 스캔은 그걸 `1` 로 읽고 `-1 0` 을
만들었다. 지금은 앞의 `-` 를 숫자의 일부로 보고, `req <= 0` 이면 거절한다.

**2. 가로선이 한 글자 왼쪽에 그어졌다.** 원인은 이 게임의 텍스트 스택 전체에
걸리는 사실이라 `CLAUDE.md` 에도 적었다:

| | 이스케이프 | 공백 | ASCII | 한글 |
|---|---|---|---|---|
| `HF_GetWidth2` (누산기 seed 1) | 0 | 4 | 5 | 9 |
| `HF_DrawTextOutlineH2` pen stride (화면 실측) | 0 | 5 | 6 | 10 |

그리기 pen 은 **글자당 1픽셀씩 더** 간다. 그래서

```
pen(i) = HF_GetWidth(s, i, 0) - 1 + (i 앞에서 그려진 글자 수)
```

이고 seed 1 이 그 중 한 글자분이다(`patch.rs::pen_offset`). 접두사가 한 글자면
오차가 0 이라 `host_cash_price` 의 주석이 "서로 상쇄된다"고 적었던 것인데, 접두사가
길면 그만큼 밀린다 — `|CASH &` 는 다섯 글자라 **4px**, `r요구레벨 &` 는 다섯
글자(한글 4 + 공백)라 역시 4~5px 이었다.

실측 (논리 픽셀, `--dump` PNG 에서 잉크 컬럼을 직접 읽음):

| | 이전 | 지금 |
|---|---|---|
| `요구레벨 1` 의 숫자 셀 | 289 (선은 285..290) | 289 (선 **289..295**) |
| `CASH 1000` 의 선 | 283..303 — 마지막 자리가 삐져나옴 | **287..311**, 네 자리 전부 |

`HF_GetWidth2` 가 seed 만 남은 구간에 0 을 돌려주는 것(1 이 아니라)도 같이 처리한다.

검증은 `the_rule_lands_on_the_pen_not_on_the_measured_width` 가 게스트의
`HF_GetWidth`/`HF_IsEscape` 로 직접 확인하고, `CS_POTION_DEBUG=1` 이 실행 중에
문자열·head/tail·pen·선 좌표를 찍는다.
