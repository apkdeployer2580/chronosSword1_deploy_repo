//! hero5 의 인게임 조이스틱 — 아트와 기하 (`CS_JOY_HERO5`).
//!
//! # 아트의 출처 — hero5 프레임버퍼에서 알파 복원했다 (2026-09-24)
//!
//! 그림 파일을 어디서 베낀 것이 아니다. hero5(`C:\Hero5NativeAPP`)의 게스트가
//! 그리는 조이스틱을 **hero5 프로브의 결정적 재생**으로 서로 다른 배경 위에
//! 세워 두고, 픽셀마다 `c = a·s + (1−a)·b` 를 최소자승으로 풀어 RGBA 를
//! 복원했다. 배경 다양성은 두 겹이다 — 같은 프레임(4128)에 스틱만 네 지점
//! ((110,120)/(260,140)/(60,190)/(190,80))에 세운 정렬 런 네 개(A_90 모드는
//! 베이스가 누른 점으로 가므로 가능), 그리고 **논리 폭 400/520 두 벌**(폭이
//! 다르면 카메라가 중앙을 지키므로 고정 앵커 밑의 배경이 통째로 바뀐다).
//! 런끼리는 스틱 상자 밖 픽셀이 **전부 동일**함을 먼저 확인했다(잡음 0).
//!
//! 복원한 세 장을 다시 합성해 원본 덤프와 대조하면 **채널 평균 오차 1.1/255**
//! (p99 ≈ 10)다. 남는 오차는 노브가 절대 비켜 주지 않는 링 중심(r ≲ 14,
//! hero5 에서도 어떤 상태로도 안 보인다)의 인페인트와 565 반올림이다.
//!
//! # 기하 — 전부 hero5 실측이다
//!
//! * 스프라이트 73×73(링 바깥 반지름 ~36.5), 노브 51×51(불투명 심 r ≤ 22,
//!   가장자리 r 22..25 페이드).
//! * 고정 앵커 **(0x41, H−0x41) = (65, H−65)** — hero5 의 B(고정) 모드가 쓰고,
//!   A(자유) 모드의 그랩 클램프도 이 좌표계다. 폭 400/520 양쪽에서 확인.
//! * 노브 디플렉션 **12 px**, 4방향 스냅(hero5 의 `*_90` 터치방식들 —
//!   B_90 의 q1..q5 실측이 정확히 (0,±12)/(±12,0)).
//! * 데드존 **±10 px**(축별), 우세축 판정, **동률은 세로가 이긴다**
//!   (hero5 실측: (35,35) 드래그 → DOWN).
//! * 자유 모드 idle 은 **아무것도 안 그린다**, 고정 모드 idle 은 같은 그림을
//!   반투명(레이어당 ×0.46 적합)으로 그린다 — 양쪽 다 hero5 실측.
//!
//! hero5 는 터치방식이 넷(B_360/A_90/B_90/A_360)이고 360 쪽은 노브가 5° 단위
//! 연속각이다. 크로노스의 걷기는 4방향뿐이라 **90 계열**을 옮긴다. hero5
//! A_90 의 「그랩 후 첫 방향 전환은 노브를 안 움직인다」는 한 틱짜리 결함은
//! 재현하지 않는다 — 눈에는 「노브가 안 움직인다」로만 읽히는 종류다.

use crate::patch::Dir;

/// `cs_core/joy/joy_hero5.bin` — 위 복원의 산출물. 형식:
/// `"CSJOY1\0\0"` 매직 뒤에 (u16 w, u16 h, RGBA8888) 세 장이
/// 링 → 노브 → idle 순서로 붙는다.
const BLOB: &[u8] = include_bytes!("../joy/joy_hero5.bin");

/// 고정 앵커의 x, 그리고 화면 아래에서 앵커까지의 거리. hero5 의
/// `(0x41, H−0x41)` 그대로다.
pub const ANCHOR_X: i32 = 65;
pub const ANCHOR_BOTTOM: i32 = 65;

/// 노브가 방향 쪽으로 밀리는 픽셀 수. hero5 B_90/A_90 실측 12.
pub const KNOB_DEFLECT: i32 = 12;

/// 축별 데드존. hero5 의 ±10 px. **잡을 때**의 문턱이다 — 잡힌 뒤에는
/// [`DEAD_KEEP`] 까지 내려와야 놓인다(아래 히스테리시스).
pub const DEAD: i32 = 10;

/// 잡힌 뒤의 놓임 문턱. 걷기는 누르는 것이 아니라 쥐고 미는 것이라, 문턱이
/// 하나뿐이면 노브 위에 얹힌 엄지가 흔들릴 때마다 ±10 경계를 넘나들며
/// release 가 나가 걷기가 뚝뚝 끊긴다 — 기기 제보 그대로다. S25 기기 실측
/// 배율(4.5x)에서 3 논리 px ≈ 1.4 mm 라, 일부러 정중앙에 되돌려 놓지 않는
/// 한 안 놓인다. 멈추는 정상 경로는 손을 떼는 것이고, 그때는 게스트가
/// 자기 release 경로로 방향을 지운다.
pub const DEAD_KEEP: i32 = 3;

/// 한 장의 스프라이트 — `data` 는 RGBA8888, 행 우선.
pub struct Sprite {
    pub w: i32,
    pub h: i32,
    pub data: &'static [u8],
}

pub struct Art {
    /// 링(베이스 판) 하나. 노브 밑(r ≲ 14)은 hero5 에서도 절대 안 보이는
    /// 영역이라 인페인트다.
    pub ring: Sprite,
    /// 노브 하나 — 링 위에 얹는다.
    pub knob: Sprite,
    /// 고정 모드의 idle 한 장(링+노브 합성, 반투명). 직접 복원이 실패한
    /// 픽셀(배경 두 벌이 우연히 같은 값이던 곳)만 레이어당 ×0.46 모델로
    /// 채웠다.
    pub idle: Sprite,
}

/// 블롭을 한 번 파싱한다. 형식이 어긋나면 `None` — 설치가 통째로 빠지고
/// 게스트가 자기 패드를 그린다.
pub fn art() -> Option<&'static Art> {
    static ART: std::sync::OnceLock<Option<Art>> = std::sync::OnceLock::new();
    ART.get_or_init(parse).as_ref()
}

fn parse() -> Option<Art> {
    let rest = BLOB.strip_prefix(b"CSJOY1\0\0")?;
    let (ring, rest) = sprite(rest)?;
    let (knob, rest) = sprite(rest)?;
    let (idle, rest) = sprite(rest)?;
    if !rest.is_empty() {
        return None;
    }
    Some(Art { ring, knob, idle })
}

fn sprite(b: &'static [u8]) -> Option<(Sprite, &'static [u8])> {
    let w = u16::from_le_bytes([*b.first()?, *b.get(1)?]) as i32;
    let h = u16::from_le_bytes([*b.get(2)?, *b.get(3)?]) as i32;
    let n = 4 + (w * h * 4) as usize;
    if b.len() < n || !(1..=256).contains(&w) || !(1..=256).contains(&h) {
        return None;
    }
    Some((Sprite { w, h, data: &b[4..n] }, &b[n..]))
}

/// hero5 의 4방향 판정: 축별 ±[`DEAD`] 데드존, 우세축, **동률은 세로**,
/// 그리고 앵커에서 `reach` 픽셀(체비셰프 거리)을 넘으면 잡지 않는다.
///
/// hero5 의 고정 모드는 걷기 영역 전체가 그랩이지만, 크로노스의
/// `GameFrame_PressTouchKeypad` 는 이 사각형 넷을 **다른 버튼들보다 먼저**
/// 재므로 무한정 넓히면 그 버튼들을 가린다. 그래서 도달 반경이 노브다
/// (`CS_JOY_REACH`, 기본 60 — 스킬 버튼 열(x ≥ 241)과 상단 바에 안 닿는다).
pub fn quantize(dx: i32, dy: i32, reach: i32) -> Option<Dir> {
    let (adx, ady) = (dx.abs(), dy.abs());
    let d = adx.max(ady);
    if d <= DEAD || d > reach {
        return None;
    }
    Some(if ady >= adx {
        if dy < 0 {
            Dir::Up
        } else {
            Dir::Down
        }
    } else if dx < 0 {
        Dir::Left
    } else {
        Dir::Right
    })
}

/// [`quantize`] 의 **잡힌 뒤** 판 — 히스테리시스가 이 함수의 전부다.
///
/// 무기억 판정 하나로는 스틱이 못 쓰인다는 것이 기기 제보였다(「이동이
/// 뚝뚝 끊긴다」). 원인은 셋 다 같은 모양이다: 노브에 얹힌 엄지의 흔들림이
/// (1) ±10 데드존을 스치거나, (2) 도달 반경을 벗어나거나, (3) 대각선
/// 경계에서 축이 널뛰면 그 틱에 release 가 나가 걷기 래치가 풀린다. 그래서
/// 잡힌 뒤에는:
///
/// * 놓임은 [`DEAD_KEEP`](3) 아래에서만 — ±10 안쪽(3..=10)에서는 **현재
///   방향을 그대로 유지**한다(그 거리의 방향 정보는 잡음이다).
/// * 도달 반경은 `keep_reach`(잡을 때의 두 배) — 밀던 엄지가 판 밖으로
///   흘러도 걷기가 이어진다. hero5 의 고정 모드는 아예 걷기 영역 전체를
///   따라간다.
/// * 축 전환은 새 축이 현재 축을 **5/4 이상** 이길 때만(TURN_RATIO) —
///   대각선 위의 흔들림이 상/우를 널뛰지 않는다. 같은 축 안의 부호 반전
///   (위→아래)은 그대로 통과한다.
pub fn quantize_held(dx: i32, dy: i32, cur: Dir, keep_reach: i32) -> Option<Dir> {
    let (adx, ady) = (dx.abs(), dy.abs());
    let d = adx.max(ady);
    if d <= DEAD_KEEP || d > keep_reach {
        return None;
    }
    if d <= DEAD {
        return Some(cur);
    }
    let vertical = if matches!(cur, Dir::Up | Dir::Down) {
        // 세로를 쥔 채: 가로가 5/4 이상 우세해야 넘어간다.
        !(adx * 4 > ady * 5)
    } else {
        ady * 4 > adx * 5
    };
    Some(if vertical {
        if dy < 0 {
            Dir::Up
        } else {
            Dir::Down
        }
    } else if dx < 0 {
        Dir::Left
    } else {
        Dir::Right
    })
}

/// `mainFrame+0x6c` 의 WIPI 방향 키(−1..−4)를 방향으로. 그 밖(−999 포함)은
/// 「방향 없음」이다.
pub fn dir_of_wipi(key: i32) -> Option<Dir> {
    match key {
        -1 => Some(Dir::Up),
        -2 => Some(Dir::Down),
        -3 => Some(Dir::Left),
        -4 => Some(Dir::Right),
        _ => None,
    }
}

/// [`dir_of_wipi`] 의 역 — 게스트의 방향 우편함(`mainFrame+0x70`)에 쓸 값.
pub fn wipi_of_dir(dir: Dir) -> i32 {
    match dir {
        Dir::Up => -1,
        Dir::Down => -2,
        Dir::Left => -3,
        Dir::Right => -4,
    }
}

/// 방향이 노브를 미는 곳.
pub fn knob_offset(dir: Option<Dir>) -> (i32, i32) {
    match dir {
        Some(Dir::Up) => (0, -KNOB_DEFLECT),
        Some(Dir::Down) => (0, KNOB_DEFLECT),
        Some(Dir::Left) => (-KNOB_DEFLECT, 0),
        Some(Dir::Right) => (KNOB_DEFLECT, 0),
        None => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_art_parses_and_is_the_shape_the_recovery_measured() {
        let a = art().expect("joy_hero5.bin parses");
        assert_eq!((a.ring.w, a.ring.h), (73, 73));
        assert_eq!((a.knob.w, a.knob.h), (51, 51));
        assert_eq!((a.idle.w, a.idle.h), (73, 73));
        let alpha = |s: &Sprite, x: i32, y: i32| s.data[((y * s.w + x) * 4 + 3) as usize];
        // 노브 심은 불투명하고(중앙), 가장자리는 페이드다.
        assert_eq!(alpha(&a.knob, 25, 25), 255);
        assert_eq!(alpha(&a.knob, 0, 0), 0, "the knob's corners are empty");
        // 링 밴드는 반투명(실측 a ≈ 0.37)이고 판 밖은 빈다.
        let band = alpha(&a.ring, 36 + 30, 36) as i32;
        assert!(
            (60..=140).contains(&band),
            "ring band alpha {band} should be near 0.37*255"
        );
        assert_eq!(alpha(&a.ring, 0, 0), 0);
        assert_eq!(alpha(&a.ring, 72, 72), 0);
        // idle 은 같은 자리가 held 보다 옅다.
        let idle_band = alpha(&a.idle, 36 + 30, 36) as i32;
        assert!(idle_band < band, "idle ({idle_band}) must be dimmer than held ({band})");
        // idle 중앙(노브 자리)도 반투명이다 — held 노브는 불투명.
        assert!(alpha(&a.idle, 36, 36) < 255);
    }

    #[test]
    fn the_quantizer_is_hero5s() {
        // 데드존: 축별 ±10.
        assert_eq!(quantize(10, 10, 60), None);
        assert_eq!(quantize(0, 0, 60), None);
        // 우세축.
        assert_eq!(quantize(45, -55, 60), Some(Dir::Up));
        assert_eq!(quantize(45, 15, 60), Some(Dir::Right));
        assert_eq!(quantize(-20, 15, 60), Some(Dir::Left));
        assert_eq!(quantize(0, 15, 60), Some(Dir::Down));
        // 동률은 세로가 이긴다 (hero5: (35,35) → DOWN).
        assert_eq!(quantize(35, 35, 60), Some(Dir::Down));
        assert_eq!(quantize(-35, -35, 60), Some(Dir::Up));
        // 도달 반경 밖은 잡지 않는다.
        assert_eq!(quantize(61, 0, 60), None);
        assert_eq!(quantize(60, 0, 60), Some(Dir::Right));
    }

    #[test]
    fn a_held_stick_only_lets_go_deliberately() {
        let keep = 120;
        // ±10 안쪽의 흔들림은 현재 방향을 그대로 유지한다 — 무기억 판이면
        // 여기서 release 가 나가 걷기가 끊긴다(기기 제보).
        for cur in [Dir::Up, Dir::Down, Dir::Left, Dir::Right] {
            assert_eq!(quantize_held(8, 0, cur, keep), Some(cur));
            assert_eq!(quantize_held(0, -8, cur, keep), Some(cur));
            assert_eq!(quantize_held(-5, 4, cur, keep), Some(cur));
        }
        // 정말 정중앙(≤3)에 되돌리면 놓인다.
        assert_eq!(quantize_held(2, 1, Dir::Right, keep), None);
        // 잡을 때의 반경(60)을 넘어도 keep 반경까지는 따라간다.
        assert_eq!(quantize(100, 0, 60), None, "engage would have let go here");
        assert_eq!(quantize_held(100, 0, Dir::Right, keep), Some(Dir::Right));
        assert_eq!(quantize_held(130, 0, Dir::Right, keep), None, "past keep_reach");
        // 축 전환은 5/4 우세부터다 — 대각선 위의 흔들림이 널뛰지 않는다.
        assert_eq!(quantize_held(20, 22, Dir::Right, keep), Some(Dir::Right));
        assert_eq!(quantize_held(20, 30, Dir::Right, keep), Some(Dir::Down));
        assert_eq!(quantize_held(22, -20, Dir::Up, keep), Some(Dir::Up));
        assert_eq!(quantize_held(30, -20, Dir::Up, keep), Some(Dir::Right));
        // 같은 축 안의 부호 반전은 그대로 통과한다.
        assert_eq!(quantize_held(0, 20, Dir::Up, keep), Some(Dir::Down));
        assert_eq!(quantize_held(-20, 0, Dir::Right, keep), Some(Dir::Left));
    }

    #[test]
    fn the_knob_moves_twelve_pixels_where_the_walk_goes() {
        assert_eq!(knob_offset(dir_of_wipi(-1)), (0, -12));
        assert_eq!(knob_offset(dir_of_wipi(-2)), (0, 12));
        assert_eq!(knob_offset(dir_of_wipi(-3)), (-12, 0));
        assert_eq!(knob_offset(dir_of_wipi(-4)), (12, 0));
        assert_eq!(knob_offset(dir_of_wipi(-999)), (0, 0));
        assert_eq!(knob_offset(dir_of_wipi(0)), (0, 0));
        for d in [Dir::Up, Dir::Down, Dir::Left, Dir::Right] {
            assert_eq!(dir_of_wipi(wipi_of_dir(d)), Some(d), "wipi mapping must round-trip");
        }
    }
}
