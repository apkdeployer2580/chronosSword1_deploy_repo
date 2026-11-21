//! 게임패드 방향 입력을 게스트가 견디는 모양으로 바꾼다.
//!
//! 이 게임의 걷기는 **래치**다. `GameFrame_gamekey` 의 점프표가 `2`/`8`/`4`/`6`
//! 을 `HERO_Key_DirectKeyPress` 로 보내고, 그 함수는 히어로가 아직 서 있을 때만
//! (`hero[0xc0] == 0`) `HERO_Set_Motion(1, dir)` + `HERO_Set_State(2)` 를 하고,
//! 이미 걷고 있으면 죽은 타임스탬프(`mainFrame+0x78` — 이미지 전체에서 **읽는
//! 곳이 없다**)만 찍고 돌아간다. 그래서 **한 번 누르면 릴리스까지 계속 걷는다** —
//! 프로브 실측: 오른쪽을 190프레임 눌러 쥐면 방을 가로질러 걸어간다.
//!
//! 그리고 방향은 정확히 **네 개**다. `HERO_Key_Convert_Dir`(0x8ac00)의 점프표는
//! `key + 4` 로 인덱싱되는데 기본값(= 현재 방향 유지)이 아닌 엔트리가 여덟 개뿐이고
//! 서로 짝이다 — `-1`/`2` → 0(위), `-2`/`8` → 1(아래), `-3`/`4` → 2(왼),
//! `-4`/`6` → 3(오른). 터치 조이스틱도 같은 네 방향으로 접힌다. **대각선은 이
//! 게임에 없다.** 그래서 자매 프로젝트(hero5)가 해야 했던 「가짜 손가락으로 게스트
//! 조이스틱을 흉내내기」가 여기서는 필요 없다 — 키가 이미 터치와 같은 곳에 닿는다.
//!
//! ## 그런데 릴리스 하나가 걷기를 취소한다
//!
//! `timerCB` 는 매 게임 틱(약 21 Hz)마다 이 순서로 돈다:
//!
//! ```text
//! MainFrame_KeyPressed()    ← mainFrame[0x5c] (press 메일박스) 를 비운다
//! MainFrame_KeyReleased()   ← mainFrame[0x58] (release 메일박스) 를 비운다
//! ... Process / Render ...
//! mainFrame[0x58] = mainFrame[0x5c] = -100      ← 틱 끝에 지운다
//! ```
//!
//! `onHandleEvent` 는 키 이벤트를 그 **한 칸짜리 메일박스**에 적기만 한다(타입 2 →
//! `[0x5c]`, 타입 3 → `[0x58]`). 즉 한 틱에 press 하나 · release 하나뿐이고, 같은
//! 틱에 둘이 다 들어오면 **press 가 먼저 소비되고 release 가 곧바로 그것을
//! 취소한다.**
//!
//! 프로브로 재현했다. 오른쪽을 쥔 채 6프레임마다 release+press 한 쌍을 끼워 넣으면
//! (안드로이드 `SyntheticJoystickHandler` 가 축을 다시 걸 때 내는 그 모양)
//! 히어로가 **제자리에서 거의 못 움직인다** — 같은 재생에서 한 번만 누른 쪽은 방을
//! 가로지른다. 이게 「패드로 이동이 뚝뚝 끊긴다」의 정체다.
//!
//! ## 그래서 이 모듈이 조건화하는 것은 **release 쪽뿐이다**
//!
//! 여기서 두 방향을 반드시 갈라 놓아야 한다 — 이 모듈의 첫 판이 그것을 섞어서
//! 두 번째 버그를 만들었다(바로 아래 절).
//!
//! * **press 는 게스트에게 해가 없다.** 걷는 중의 같은 방향 press 는 위 래치에서
//!   죽은 타임스탬프 한 번이고(프로브 실측: 2프레임마다 다시 눌러도 덤프 픽셀이
//!   완전히 동일), 메뉴 화면에서는 그 숫자가 **커서 한 칸**이다.
//! * **release 는 해가 있다.** 걷기를 취소하는 것이 그것 하나다.
//!
//! 그래서:
//!
//! * **오토리피트는 버린다**(아래 「되살리면 안 된다」).
//! * **누르고 있는 방향을 집합으로 들고**, 활성 방향은 **가장 최근에 눌린 것**이다.
//!   걷는 중에 새 방향을 눌러도 게스트가 방향을 바꿔 주므로(프로브 실측) 그 앞에
//!   release 를 끼우지 않는다. 끼우는 것이 바로 위 버그다.
//! * **release 는 아무 방향도 안 눌려 있을 때만** 내고, 그것도
//!   [`RELEASE_GRACE_MS`] 만큼 **미룬다** — 그 창 안에 아무 방향이든 다시 눌리면
//!   취소한다. 프레임워크가 축을 다시 걸 때 내는 release+press 의 release 쪽이
//!   여기서 죽는다.
//!
//! 방향 아닌 키(공격·스킬·메뉴)는 **손대지 않고 그대로 흘린다**.
//!
//! ## 눌린 것은 언제나 게스트에 간다 (2026-09-10)
//!
//! 첫 판은 「게스트가 이미 그 방향으로 걷고 있으면 press 를 안 낸다」였다. 걷기만
//! 보면 맞는 말인데 **같은 네 코드가 메뉴에서는 래치가 아니다** — 이 저장소가
//! 「메뉴에서는 숫자가 화살표와 완전히 같다」고 적어 둔 그 `2/8/4/6` 이고, 거기서
//! press 하나는 커서 한 칸이다. 그 판을 떼어내 돌리면(탭마다 40 ms 쥐고 10회):
//!
//! ```text
//! 탭 간격 10 ms →  1/10 회만 게스트에 도달
//! 탭 간격 30 ms →  1/10
//! 탭 간격 50 ms →  1/10
//! 탭 간격 60 ms → 10/10      ← 절벽이 RELEASE_GRACE_MS 에 정확히 있었다
//! ```
//!
//! 기전은 「release 가 유예를 걸고, 그 안에 같은 코드가 다시 눌리면 유예가
//! 취소되는데 `sent` 는 그대로」다. 그러면 `sent == want` 라 press 가 안 나가고,
//! **그 리듬이 유지되는 한 `sent` 는 영영 안 지워진다** — 두 번째 탭만 잃는 게
//! 아니라 그 뒤 전부다. 게다가 60 ms 는 사람이 연타하는 구간(빠른 연타의
//! release→press 간격 50~70 ms) 한가운데다.
//!
//! 지금은 **물리적 키다운 엣지마다 press 를 낸다.** 대가는 위 프로브 실측이 이미
//! 재어 놓았고(걷기에서 무해), 빈도는 이 커밋 이전 빌드와 같다. 얻는 것은 메뉴
//! 입력이 안 사라지는 것, 그리고 [`RELEASE_GRACE_MS`] 값이 더 이상 예민하지 않은
//! 것이다 — 연타 중에 삼켜지는 중간 release 는 「걷기가 이어진다」일 뿐이고,
//! 마지막 release 는 그대로 멈춘다.
//!
//! ## 오토리피트를 되살리면 안 된다
//!
//! 안드로이드는 방향키를 쥐고 있으면 약 400 ms 뒤부터 **초당 20번** 리피트를
//! 보낸다. 게스트에는 그것을 받을 채널이 없다 — `onHandleEvent` 가 아는 이벤트
//! 타입은 press(2)와 release(3) 둘이고, 원본 폰의 키패드에도 리피트가 없어서
//! 애초에 걷기가 래치인 것이다. 그러니 리피트는 **press 로** 흘릴 수밖에 없는데,
//! 게임 틱이 약 47 ms 라 그러면 **틱마다 방향 press 하나**가 메일박스에 앉는다.
//! 그 칸은 하나고 마지막에 쓴 것이 이긴다 — 즉 **걸으면서 스킬을 쓰면 그 스킬
//! press 의 절반쯤이 방향에 덮여 사라진다.** 전투가 정확히 그 조합이다.
//!
//! 대가는 「메뉴에서 쥐고 있으면 스크롤되던 것」이고, 그건 원본 폰에도 없던
//! 동작이다(게임 자신의 터치 화살표도 탭마다 한 칸이다). 위 수정으로 **아무리
//! 빨리 두드려도 전부 도달**하므로 남는 것은 취향이고, 잃는 쪽이 훨씬 비싸다.

/// 미뤄 둔 release 를 실제로 보낼 때까지의 유예.
///
/// 조건은 아래위 둘뿐이다:
///
/// * **아래** — 프레임워크가 축을 다시 걸 때 내는 release→press 간격보다 커야
///   한다. 그 값은 **아직 측정된 적이 없다.** 한 쌍이
///   `ViewRootImpl.SyntheticJoystickHandler` 의 같은 `update()` 에서 나오므로 같은
///   입력 배치, 즉 호스트 프레임 하나 안에 도착할 것으로 보고 게임 틱 하나(약
///   47 ms)에 여유를 붙여 잡았다.
/// * **위** — 손을 뗀 것이 얼마나 늦어도 되는지. 두 틱을 넘으면 「놓아도 안
///   멈춘다」로 읽힌다. 자매 프로젝트가 남긴 교훈대로 기기 시험의 첫 항목은
///   「스틱으로 걷는가」가 아니라 **「놓으면 멈추는가」** 다.
///
/// 사람의 연타 리듬은 이제 이 값과 무관하다 — press 는 유예를 보지 않는다.
pub const RELEASE_GRACE_MS: i64 = 60;

/// 게스트의 네 방향 키. `HERO_Key_Convert_Dir` 의 기본값 아닌 엔트리 여덟 개 중
/// 숫자 쪽 절반이고, WIPI 화살표 `-1..-4` 와 같은 방향으로 접힌다.
pub const UP: i32 = b'2' as i32;
pub const DOWN: i32 = b'8' as i32;
pub const LEFT: i32 = b'4' as i32;
pub const RIGHT: i32 = b'6' as i32;

/// 이 코드가 걷기 방향인가. 방향이 아닌 것은 이 모듈을 통과하지 않는다.
pub fn is_direction(code: i32) -> bool {
    matches!(code, UP | DOWN | LEFT | RIGHT)
}

// ---- 어깨·스틱 버튼 여섯 개: 표는 하나다 -----------------------------------

/// 패드 버튼의 **자리**. 글자가 아닌 이유는 안드로이드가 face 버튼을 위치로
/// 보고하기 때문이다 — 커널의 `BTN_SOUTH` 가 키레이아웃을 지나
/// `KEYCODE_BUTTON_A`(96) 가 되므로 96은 어느 패드에서나 「아래쪽 face」지만,
/// 인쇄된 글자는 Xbox `A` / 스위치 `B`(닌텐도는 좌우가 뒤집혔다) / 플레이스테이션은
/// 아예 없음으로 갈린다. 그러니 `A` 라고 쓴 배지는 세 집안 중 둘에서 틀린다.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PadPos {
    /// 어깨 넷 중 하나. `right` = 오른쪽, `lower` = 아래쪽(L2/R2).
    Shoulder { right: bool, lower: bool },
    /// 스틱 누르기 둘 중 하나.
    Stick { right: bool },
}

/// 어깨·스틱 버튼 하나 — 안드로이드 키코드, 그것이 게스트에 쏘는 키, 손에서의 자리.
pub struct Slot {
    /// `KEYCODE_BUTTON_L1`..`R2`, `THUMBL`/`THUMBR`.
    pub android: u32,
    /// 게스트의 스킬 단축슬롯 키. 피처폰 키패드가 그것들을 부르던 이름이다.
    pub key: i32,
    /// 배지가 그리는 자리.
    pub pos: PadPos,
}

/// **표는 하나다.** 이것이 안드로이드 키코드 → 게스트 키(`cs_android::wipi_key`)와
/// 게스트 키 → 배지 자리(`patch::draw_pad_hud`) **양쪽의 출처**다. 표를 둘 두면
/// 언젠가 갈리고, 갈리면 배지가 다른 일을 하는 버튼을 가리킨다 — 그리고 그것을
/// 잡아 줄 것이 아무것도 없다(한쪽은 안드로이드 전용 크레이트라 유닛 테스트가
/// 양쪽을 같이 볼 수 없다). 그래서 컴파일 타임 사실로 만들어 둔다.
///
/// 여섯 슬롯의 순서는 **게스트의 키 표 순서가 아니다** — 그쪽은 `.rodata` 의
/// `1 3 7 9 * 0` 이고 화면의 여섯 칸이 그 순서로 앉는다(`patch::PAD_SLOT_KEYS`).
/// 여기 순서는 손에서의 순서(L1 R1 L2 R2 L3 R3)다. **둘을 나란히 놓고 읽지 말
/// 것**: 화면 다섯째 칸은 `'*'` = R3(오른 스틱)이고 여섯째가 `'0'` = L3 이다.
pub const SLOTS: [Slot; 6] = [
    Slot { android: 102, key: b'1' as i32, pos: PadPos::Shoulder { right: false, lower: false } },
    Slot { android: 103, key: b'3' as i32, pos: PadPos::Shoulder { right: true, lower: false } },
    Slot { android: 104, key: b'7' as i32, pos: PadPos::Shoulder { right: false, lower: true } },
    Slot { android: 105, key: b'9' as i32, pos: PadPos::Shoulder { right: true, lower: true } },
    Slot { android: 106, key: b'0' as i32, pos: PadPos::Stick { right: false } },
    Slot { android: 107, key: b'*' as i32, pos: PadPos::Stick { right: true } },
];

/// 이 안드로이드 키코드가 쏘는 게스트 키.
pub fn key_for_android(code: u32) -> Option<i32> {
    SLOTS.iter().find(|s| s.android == code).map(|s| s.key)
}

/// 이 게스트 키를 쏘는 패드 버튼의 자리.
pub fn pos_for_key(key: i32) -> Option<PadPos> {
    SLOTS.iter().find(|s| s.key == key).map(|s| s.pos)
}

/// [`Pad::key`]/[`Pad::tick`] 이 게스트에게 보내라고 답하는 것.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Emit {
    /// 아무것도 보내지 않는다.
    None,
    /// 이 코드로 `MV_KEY_PRESS`.
    Press(i32),
    /// 이 코드로 `MV_KEY_RELEASE`.
    Release(i32),
}

/// 눌려 있는 방향들, 그리고 미뤄 둔 release 하나.
///
/// 순수 상태기계다 — 시각을 인자로 받고 I/O 를 하지 않으므로 유닛 테스트가 기기
/// 없이 전 경로를 돌릴 수 있다.
#[derive(Default)]
pub struct Pad {
    /// 눌린 순서대로. 마지막이 활성 방향이다. 방향이 넷뿐이라 `Vec` 이면 충분하고,
    /// 순서가 곧 「가장 최근」이라는 정보다.
    held: Vec<i32>,
    /// 게스트가 지금 걷고 있다고 알고 있는 방향. **press 를 낼지 말지를 이 값으로
    /// 정하지 않는다** — 그것이 위 「눌린 것은 언제나」 절의 버그였다. 이 값이
    /// 답하는 것은 「release 를 갚아야 하는가」 하나뿐이다.
    sent: Option<i32>,
    /// `(방향, 보낼 시각)`. [`Pad::tick`] 이 만기를 처리한다.
    pending: Option<(i32, i64)>,
}

impl Pad {
    /// 방향키 하나의 엣지. `now_ms` 는 벽시계(또는 프로브의 가상 시계) 밀리초.
    ///
    /// 방향이 아닌 코드는 여기 오면 안 된다 — 호출자가 [`is_direction`] 으로
    /// 먼저 가른다.
    pub fn key(&mut self, code: i32, down: bool, repeat: bool, now_ms: i64) -> Emit {
        // 오토리피트: 게스트에 그것을 받을 이벤트 타입이 없고, press 로 흘리면
        // 틱마다 메일박스를 먹어 같은 틱의 스킬 press 를 덮는다(모듈 주석).
        if repeat {
            return Emit::None;
        }
        if !down {
            if !self.held.contains(&code) {
                // 누른 적 없는 것의 릴리스. 프레임워크가 축을 다시 걸 때, 포커스를
                // 잃었다 돌아올 때, 또는 IME 가 그 사이 화살표를 가져갔다 돌려줬을
                // 때 온다. 상태를 흔들지 않는다.
                return Emit::None;
            }
            self.held.retain(|&c| c != code);
            return self.settle(now_ms);
        }
        // 눌렸다. 미뤄 둔 release 는 그것이 무엇이었든 무효다 — 이 프레임에 이미
        // 어느 방향이든 걷기 시작하기 때문이다.
        self.pending = None;
        // 리피트로 표시되지 않은 재-누름도 순서만 최신으로 올린다.
        self.held.retain(|&c| c != code);
        self.held.push(code);
        // 그리고 **언제나** 게스트에 간다. 걷는 중이면 죽은 타임스탬프 한 번이고,
        // 메뉴에서는 플레이어가 요구한 커서 한 칸이다.
        self.sent = Some(code);
        Emit::Press(code)
    }

    /// 매 호스트 프레임. 미뤄 둔 release 의 만기만 본다.
    pub fn tick(&mut self, now_ms: i64) -> Emit {
        match self.pending {
            Some((code, due)) if now_ms >= due => {
                self.pending = None;
                if self.sent == Some(code) {
                    self.sent = None;
                    Emit::Release(code)
                } else {
                    Emit::None
                }
            }
            _ => Emit::None,
        }
    }

    /// 포커스를 잃었을 때, 그리고 IME 가 화살표를 돌려줄 때. 아직 눌려 있다고
    /// 믿는 것을 놓아 준다 — 안 그러면 히어로가 남은 세션 내내 벽으로 걸어간다.
    pub fn release_all(&mut self) -> Emit {
        self.held.clear();
        self.pending = None;
        match self.sent.take() {
            Some(code) => Emit::Release(code),
            None => Emit::None,
        }
    }

    /// IME 가 화살표를 가져갔다 — 잡고 있던 **제스처를 버린다**.
    ///
    /// 오버레이가 떠 있는 동안 방향은 `-1..-4` 로 게스트에 직행하므로
    /// (`cs_android::wipi_key`), 오버레이 전에 시작한 제스처의 release 는 이
    /// 상태기계에 **영영 안 온다**. 그 찌꺼기를 그대로 두면 나중에 **아무** 방향의
    /// release 하나가 `held` 의 마지막 값으로 되돌아가 히어로가 저절로 걸어간다.
    ///
    /// 게스트가 무엇을 듣고 있는지(`sent`)는 **남긴다** — 오버레이가 닫힐 때
    /// [`Pad::release_all`] 이 그것을 갚는다. 여기서 release 를 내지 않는 이유는
    /// 그 순간 키 이벤트가 게스트 IME 의 것이라서다(`clsidAuthHandle+0x104`).
    pub fn forget(&mut self) {
        self.held.clear();
        self.pending = None;
    }

    /// 지금 눌려 있는 것과 게스트가 아는 것을 맞춘다. **키업 엣지 전용**이다 —
    /// 키다운은 조건 없이 press 를 내므로 여기를 지나지 않는다.
    fn settle(&mut self, now_ms: i64) -> Emit {
        match self.held.last().copied() {
            // 아직 방향이 눌려 있다: 그것으로 걷고 있어야 한다. 미뤄 둔 release 는
            // 무효다 — 이 프레임에 이미 그 방향을 걷기 때문이다.
            Some(want) => {
                self.pending = None;
                if self.sent == Some(want) {
                    Emit::None
                } else {
                    self.sent = Some(want);
                    Emit::Press(want)
                }
            }
            // 아무것도 안 눌려 있다: 멈춰야 하지만 **바로는 아니다**.
            None => {
                if let Some(code) = self.sent {
                    if self.pending.is_none() {
                        self.pending = Some((code, now_ms + RELEASE_GRACE_MS));
                    }
                }
                Emit::None
            }
        }
    }

    /// 게스트가 지금 걷고 있다고 아는 방향. 테스트와 로그용.
    pub fn walking(&self) -> Option<i32> {
        self.sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_direction_is_pressed_once_and_never_released() {
        let mut p = Pad::default();
        assert_eq!(p.key(RIGHT, true, false, 0), Emit::Press(RIGHT));
        // 오토리피트는 게스트에 다시 안 간다 — 메일박스가 한 칸이라 그것이 곧
        // 같은 틱의 스킬 press 를 덮는 것이다.
        for t in 1..40 {
            assert_eq!(p.key(RIGHT, true, true, t), Emit::None);
            assert_eq!(p.tick(t), Emit::None);
        }
        assert_eq!(p.walking(), Some(RIGHT));
    }

    /// 이 패치의 이유 그 자체 — 프레임워크가 축을 다시 걸 때 내는 release+press.
    /// **release 쪽이** 유예에서 죽어야 걷기가 안 끊긴다. press 쪽은 지나가도
    /// 되고(걷는 중의 같은 방향 press 는 프로브 실측으로 픽셀 동일), 지나가야
    /// 한다 — 안 그러면 같은 코드가 메뉴에서 씹힌다.
    #[test]
    fn the_framework_relatching_an_axis_never_releases_the_walk() {
        let mut p = Pad::default();
        assert_eq!(p.key(RIGHT, true, false, 0), Emit::Press(RIGHT));
        let mut t = 0;
        for _ in 0..20 {
            t += 100;
            assert_eq!(p.key(RIGHT, false, false, t), Emit::None, "release leaked at {t}");
            // 유예가 끝나기 전에 다시 눌린다.
            assert_eq!(p.tick(t + 1), Emit::None);
            assert_eq!(p.key(RIGHT, true, false, t + 2), Emit::Press(RIGHT));
            assert_eq!(p.tick(t + 2 + RELEASE_GRACE_MS), Emit::None, "release leaked at {t}");
        }
        assert_eq!(p.walking(), Some(RIGHT), "the walk was cancelled");
    }

    /// 연타는 **간격이 얼마든** 전부 도달해야 한다. 메뉴에서 같은 네 코드는 래치가
    /// 아니라 커서 한 칸이고, 첫 판은 유예 안쪽 간격에서 두 번째부터 전부 씹혔다.
    #[test]
    fn taps_in_a_rhythm_each_reach_the_guest() {
        for gap in [1, 10, 30, 50, RELEASE_GRACE_MS, 200] {
            let mut p = Pad::default();
            let mut t = 0;
            let mut releases = 0;
            for n in 0..10 {
                assert_eq!(p.key(DOWN, true, false, t), Emit::Press(DOWN), "tap {n} at gap {gap}");
                t += 40; // 손가락이 닿아 있는 시간
                assert_eq!(p.key(DOWN, false, false, t), Emit::None);
                // 그 사이 호스트 프레임들이 돈다. 간격이 유예보다 넓으면 여기서
                // release 가 나가는 것이 **맞다** — 손을 200 ms 뗐으면 히어로는
                // 멈춰야 한다. 안 되는 것은 그것이 **다음 press 를 막는** 것이다.
                for f in 0..=gap / 16 {
                    if let Emit::Release(c) = p.tick(t + 16 * f) {
                        assert_eq!(c, DOWN);
                        releases += 1;
                    }
                }
                t += gap;
            }
            // 마지막 놓은 것은 반드시 멈춘다 — 여기서든, 위 루프에서든.
            if let Emit::Release(c) = p.tick(t + RELEASE_GRACE_MS) {
                assert_eq!(c, DOWN);
                releases += 1;
            }
            assert_eq!(p.walking(), None, "gap {gap}: the hero never stopped");
            assert!(releases >= 1, "gap {gap}: nothing ever stopped the hero");
            // 유예보다 좁은 리듬에서는 걷기가 이어진다 — 중간 release 는 삼켜지고
            // 마지막 하나만 나간다.
            if gap < RELEASE_GRACE_MS {
                assert_eq!(releases, 1, "gap {gap}: the walk was cut mid-rhythm");
            }
        }
    }

    #[test]
    fn letting_go_stops_the_hero_once_the_grace_expires() {
        let mut p = Pad::default();
        assert_eq!(p.key(RIGHT, true, false, 0), Emit::Press(RIGHT));
        assert_eq!(p.key(RIGHT, false, false, 10), Emit::None);
        // 유예 안에서는 아직 아니다.
        assert_eq!(p.tick(10 + RELEASE_GRACE_MS - 1), Emit::None);
        assert_eq!(p.tick(10 + RELEASE_GRACE_MS), Emit::Release(RIGHT));
        assert_eq!(p.walking(), None);
        // 그리고 한 번만이다.
        assert_eq!(p.tick(10_000), Emit::None);
    }

    /// 걷는 중에 새 방향을 누르면 게스트가 방향을 바꿔 준다(프로브 실측) —
    /// 그러니 앞에 release 를 끼우지 않는다. 끼우면 그것이 바로 이 버그다.
    #[test]
    fn turning_while_walking_sends_one_press_and_no_release() {
        let mut p = Pad::default();
        assert_eq!(p.key(RIGHT, true, false, 0), Emit::Press(RIGHT));
        assert_eq!(p.key(UP, true, false, 10), Emit::Press(UP));
        assert_eq!(p.tick(1000), Emit::None, "a release slipped in mid-turn");
        // 위를 놓으면 아직 눌려 있는 오른쪽으로 되돌아간다 — 역시 release 없이.
        assert_eq!(p.key(UP, false, false, 20), Emit::Press(RIGHT));
        assert_eq!(p.tick(1000), Emit::None);
        assert_eq!(p.key(RIGHT, false, false, 30), Emit::None);
        assert_eq!(p.tick(30 + RELEASE_GRACE_MS), Emit::Release(RIGHT));
    }

    /// 활성이 아닌 방향을 놓는 것은 **아무것도 아니다** — 키업 엣지는 활성 방향이
    /// 실제로 바뀔 때만 press 를 낸다. (키다운과 달리 여기서 press 를 내면 걷고
    /// 있는 방향이 이유 없이 다시 찍힌다.)
    #[test]
    fn releasing_a_direction_that_was_not_the_active_one_says_nothing() {
        let mut p = Pad::default();
        assert_eq!(p.key(RIGHT, true, false, 0), Emit::Press(RIGHT));
        assert_eq!(p.key(UP, true, false, 10), Emit::Press(UP));
        assert_eq!(p.key(RIGHT, false, false, 20), Emit::None, "UP was and stays the active one");
        assert_eq!(p.walking(), Some(UP));
        assert_eq!(p.tick(1000), Emit::None);
    }

    #[test]
    fn a_release_for_something_never_pressed_changes_nothing() {
        let mut p = Pad::default();
        assert_eq!(p.key(RIGHT, true, false, 0), Emit::Press(RIGHT));
        assert_eq!(p.key(LEFT, false, false, 5), Emit::None);
        assert_eq!(p.tick(1000), Emit::None);
        assert_eq!(p.walking(), Some(RIGHT));
    }

    #[test]
    fn losing_focus_lets_go_of_the_hero() {
        let mut p = Pad::default();
        assert_eq!(p.key(DOWN, true, false, 0), Emit::Press(DOWN));
        assert_eq!(p.release_all(), Emit::Release(DOWN));
        assert_eq!(p.walking(), None);
        // 두 번 부르면 아무것도 아니다.
        assert_eq!(p.release_all(), Emit::None);
    }

    /// IME 가 화살표를 가져가면 그 제스처의 release 는 이 상태기계에 안 온다.
    /// 찌꺼기가 남으면 **다른** 방향을 한 번 눌렀다 놓는 것만으로 히어로가 옛
    /// 방향으로 걸어간다 — 그것이 이 `forget` 이 막는 것이다.
    #[test]
    fn the_ime_taking_the_arrows_drops_the_gesture() {
        let mut p = Pad::default();
        assert_eq!(p.key(RIGHT, true, false, 0), Emit::Press(RIGHT));
        // 오버레이가 뜬다. 릴리스는 `-4` 로 게스트에 직행했고 여기엔 안 온다.
        p.forget();
        // 게스트가 듣고 있는 것은 아직 남아 있다 — 오버레이가 닫힐 때 갚는다.
        assert_eq!(p.walking(), Some(RIGHT));
        assert_eq!(p.release_all(), Emit::Release(RIGHT));
        // 그 뒤의 다른 방향 한 번은 자기 것만 한다.
        assert_eq!(p.key(LEFT, true, false, 100), Emit::Press(LEFT));
        assert_eq!(p.key(LEFT, false, false, 110), Emit::None);
        assert_eq!(p.tick(110 + RELEASE_GRACE_MS), Emit::Release(LEFT));
        assert_eq!(p.walking(), None, "a stale held direction resurrected the walk");
    }

    #[test]
    fn only_the_four_walking_keys_go_through_here() {
        for c in [UP, DOWN, LEFT, RIGHT] {
            assert!(is_direction(c));
        }
        // 공격·스킬·메뉴는 손대지 않는다 — 메일박스가 한 칸이라 방향을 덧씌우면
        // 같은 틱의 스킬 press 가 씹힌다.
        let others = [
            b'5' as i32,
            b'1' as i32,
            b'3' as i32,
            b'7' as i32,
            b'9' as i32,
            b'0' as i32,
            b'*' as i32,
            b'#' as i32,
            -5,
            -6,
            -7,
            -10,
            -16,
        ];
        for c in others {
            assert!(!is_direction(c), "{c} must not be conditioned as a direction");
        }
    }

    /// 여섯 슬롯 표가 양방향으로 단사이고, 자리가 어깨 넷 + 스틱 둘이다. 배지와
    /// 키 매핑이 **이 표 하나**에서 나오므로 여기서 깨지면 둘이 같이 깨진다.
    #[test]
    fn the_six_shoulder_and_stick_slots_are_one_table_both_ways() {
        for s in &SLOTS {
            assert_eq!(key_for_android(s.android), Some(s.key));
            assert_eq!(pos_for_key(s.key), Some(s.pos));
            // 스킬 슬롯 키는 방향이 아니다 — 방향이면 조건화 경로로 새 버린다.
            assert!(!is_direction(s.key), "slot key {} is a walking direction", s.key);
        }
        for i in 0..SLOTS.len() {
            for j in i + 1..SLOTS.len() {
                assert_ne!(SLOTS[i].android, SLOTS[j].android);
                assert_ne!(SLOTS[i].key, SLOTS[j].key);
                assert_ne!(SLOTS[i].pos, SLOTS[j].pos, "two slots draw the same badge");
            }
        }
        assert_eq!(SLOTS.iter().filter(|s| matches!(s.pos, PadPos::Shoulder { .. })).count(), 4);
        assert_eq!(SLOTS.iter().filter(|s| matches!(s.pos, PadPos::Stick { .. })).count(), 2);
        // 아무 코드나 답하지 않는다 — A(96) 는 이 표의 것이 아니다.
        assert_eq!(key_for_android(96), None);
        assert_eq!(pos_for_key(b'5' as i32), None);
    }
}
