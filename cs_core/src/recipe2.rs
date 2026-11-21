//! 「레시피2」 — 상점이 팔지 않는 레시피 열 개를 파는 두 번째 레시피 줄.
//!
//! 게임에는 레시피가 **110개**(아이템 종류 0x2d, sub 0..109) 있는데 마을 NPC 의
//! 「레시피」 줄이 파는 것은 **0..99** 뿐이다. 나머지 열 개 — 퀘스트 레시피 넷과
//! 파거스 방어구 세트 여섯 — 은 아이템 DB 에 이름·설명·템플릿이 다 있는데도
//! 살 곳이 없다.
//!
//! # 왜 「덧붙이기」가 안 되는가
//!
//! 캐시상점에 비약 넷을 더한 `patch::host_cash_potion_list` 는 게스트가 파싱한
//! `_merchant` 뒤에 줄을 덧붙인다. 레시피 줄에는 그 수법을 못 쓴다 —
//! **`_merchant` 는 정확히 100칸이고 레시피 파일은 정확히 100줄이다.**
//!
//! `_merchant`(0x173390)의 크기가 0x7660 이고, 배치는 100칸짜리 병렬 배열
//! 일곱 개가 **빈틈없이 맞물린 것**이다:
//!
//! ```text
//!   개수  +0      (u16)
//!   id    +2      +  100          = +0x66
//!   sub   +0x66   +  100          = +0xca
//!   이름  +0xca   + 100 * 0x28    = +0x106a   (+2 패딩)
//!   가격  +0x106c + 100 * 4       = +0x11fc
//!   수량  +0x11fc + 100           = +0x1260
//!   설명  +0x1260 + 100 * 0xff    = +0x75fc
//!   등급  +0x75fc + 100           = +0x7660  = 전체 크기
//! ```
//!
//! 101번째 줄의 id 를 쓰면 그 바이트가 곧 sub[0] 이다. 게스트 자신도 100 을
//! 상한으로 안다 — `NpcUI_setMerchant+0x224` 의 `cmp r2,#0x63 / bgt` 가
//! **개수가 100이면 id 배열의 0xff 종결자를 아예 안 쓴다**. 쓸 자리가 없어서다.
//!
//! 그래서 CS2 모드가 「레시피1/2」로 **상점 데이터 파일을 둘로 쪼갠** 것이고,
//! 여기서도 같은 답을 낸다: 카테고리 메뉴에 다섯째 줄을 더하고, 그 줄이 고르는
//! 목록은 파일이 아니라 호스트가 만든다(`patch::host_recipe2_load`).
//!
//! # 줄 내용은 하드코딩하지 않는다
//!
//! 무엇이 「안 팔리는 레시피」인지는 표로 옮겨 적지 않고 **게임 데이터 두 개를
//! 맞대서** 낸다 — `NPC/MERCHANT_1` 이 파는 sub 집합과, 아이템 이름 파일
//! `ITEM/S004` 서브블롭 5(= 종류 45)가 가진 레코드 전부. 그 차집합이 이 줄이다.
//! `netitem.rs` 가 「무엇이 네트워크 아이템인가」를 그렇게 내는 것과 같은 이유다:
//! 표가 데이터에 있는데 옮겨 적으면 옮겨 적은 쪽이 틀린다.
//!
//! 이름·설명도 그 파일에서 그대로 온다. 이름 앞의 `|` 한 글자만 뗀다 — 그것은
//! 폭 0 짜리 색 이스케이프이고 출고 상점 줄의 이름에는 붙어 있지 않다(실측:
//! `ITEM/S004` 의 `|레시피:생명의오브` ↔ `NPC/MERCHANT_1` 의 `레시피:생명의오브`).

use crate::files::{vfs_hash, VfsIndex};

/// 레시피의 아이템 종류. `ITEM/S%03d` 는 종류 k 를 파일 k/10 의 서브블롭 k%10
/// 에 담는다 — `netitem.rs` 의 `ITEM/%03d` 와 같은 병합 컨테이너 규칙이다.
pub const RECIPE_KIND: u8 = 0x2d;

/// 출고 레시피 상점. 이 파일이 파는 sub 가 「이미 팔리는 것」이다.
const RECIPE_SHOP: &[u8] = b"NPC/MERCHANT_1";

/// 상점 한 페이지는 4열 x 5행이다. 이 줄이 그보다 길어지면 페이지 UI 를
/// 기기에서 확인해야 하므로 거기서 자른다.
const PAGE: usize = 20;

/// 이름 칸(`_merchant + 0xca`, stride 0x28)과 설명 칸(stride 0xff)의 상한.
/// NUL 자리를 남겨야 하므로 **미만**이다.
const NAME_MAX: usize = 0x28;
const DESC_MAX: usize = 0xff;

/// 상점에 걸릴 한 줄.
pub struct Row {
    /// 아이템 sub-type. id 는 언제나 [`RECIPE_KIND`] 다.
    pub sub: u8,
    /// EUC-KR. `String` 을 거치지 않는다 — CLAUDE.md 의 그 규칙.
    pub name: Vec<u8>,
    pub desc: Vec<u8>,
}

/// 부팅 때 한 번 만들어 두는 목록.
#[derive(Default)]
pub struct Recipes {
    rows: Vec<Row>,
}

impl Recipes {
    /// 아카이브에서 두 파일을 읽어 차집합을 만든다. 어느 쪽이든 형식이
    /// 어긋나면 **빈 목록**을 낸다 — 그러면 다섯째 줄 자체가 설치되지 않고
    /// 상점은 출고 그대로다.
    pub fn build(vfs: &[u8], index: &VfsIndex) -> Self {
        let Some(sold) = sold_subs(vfs, index) else { return Self::default() };
        let Some(all) = every_recipe(vfs, index) else { return Self::default() };
        let mut rows = Vec::new();
        for Row { sub, name, desc } in all {
            if sold.contains(&sub) {
                continue;
            }
            // 칸에 안 들어가는 문자열은 자르지 않고 그 줄을 버린다. 잘린 EUC-KR
            // 은 반쪽 글자로 끝나고, 그 바이트가 무엇으로 그려질지는 폰트만 안다.
            if name.is_empty() || name.len() >= NAME_MAX || desc.len() >= DESC_MAX {
                log::warn!(
                    "recipe2: sub {sub} has a {}/{} byte name/desc; skipped",
                    name.len(),
                    desc.len()
                );
                continue;
            }
            rows.push(Row { sub, name, desc });
        }
        if rows.len() > PAGE {
            log::warn!(
                "recipe2: {} unsold recipes is more than one page; keeping {PAGE}",
                rows.len()
            );
            rows.truncate(PAGE);
        }
        Self { rows }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// `NPC/MERCHANT_1` 이 파는 레시피 sub 들.
///
/// 레코드 형식은 `NpcUI_setMerchant` 의 파싱 루프 그대로:
/// `u16 개수`, 그 뒤로 `u8 id, u8 sub, i16 이름길이, 이름, i32 가격, u8 수량,
/// i16 설명길이, 설명, u8 등급색`.
fn sold_subs(vfs: &[u8], index: &VfsIndex) -> Option<Vec<u8>> {
    let (off, len) = index.get(vfs_hash(RECIPE_SHOP))?;
    let f = vfs.get(off as usize..(off + len) as usize)?;
    let count = u16::from_le_bytes(f.get(..2)?.try_into().ok()?) as usize;
    let mut p = 2usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let id = *f.get(p)?;
        let sub = *f.get(p + 1)?;
        p = skip_str16(f, p.checked_add(2)?)?;
        p = p.checked_add(4 + 1)?; // 가격 + 수량
        p = skip_str16(f, p)?;
        p = p.checked_add(1)?; // 등급색
        if id == RECIPE_KIND {
            out.push(sub);
        }
    }
    // 파일을 남김없이 소비하지 못했으면 형식을 잘못 읽은 것이다.
    (p == f.len()).then_some(out)
}

/// `[i16 길이][바이트]` 를 건너뛴다.
fn skip_str16(f: &[u8], p: usize) -> Option<usize> {
    let n = i16::from_le_bytes(f.get(p..p.checked_add(2)?)?.try_into().ok()?);
    let n = usize::try_from(n).ok()?;
    let end = p.checked_add(2)?.checked_add(n)?;
    (end <= f.len()).then_some(end)
}

/// `ITEM/S004` 서브블롭 5 — 레시피 110개의 이름과 설명.
///
/// 형식은 `[u16 개수]` 뒤로 `[u8 길이][이름][u8 길이][설명]` 이다.
fn every_recipe(vfs: &[u8], index: &VfsIndex) -> Option<Vec<Row>> {
    let name = format!("ITEM/S{:03}", RECIPE_KIND / 10);
    let (off, len) = index.get(vfs_hash(name.as_bytes()))?;
    let file = vfs.get(off as usize..(off + len) as usize)?;
    let blob = merged_sub(file, (RECIPE_KIND % 10) as usize)?;
    let count = u16::from_le_bytes(blob.get(..2)?.try_into().ok()?) as usize;
    let mut p = 2usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let (name, q) = str8(blob, p)?;
        let (desc, q) = str8(blob, q)?;
        p = q;
        // 인덱스가 곧 sub 이고 sub 은 바이트다. 넘치면 그 종류를 통째로 버린다 —
        // 잘못 읽고 있다는 뜻이다.
        let sub = u8::try_from(i).ok()?;
        out.push(Row { sub, name: strip_colour(name), desc: desc.to_vec() });
    }
    (p == blob.len()).then_some(out)
}

/// `[u8 길이][바이트]`.
fn str8(b: &[u8], p: usize) -> Option<(&[u8], usize)> {
    let n = *b.get(p)? as usize;
    let end = p.checked_add(1)?.checked_add(n)?;
    Some((b.get(p + 1..end)?, end))
}

/// 이름 앞의 색 이스케이프 한 글자를 뗀다. 출고 상점 줄에는 없다.
fn strip_colour(name: &[u8]) -> Vec<u8> {
    match name.first() {
        Some(b'|') => name[1..].to_vec(),
        _ => name.to_vec(),
    }
}

/// 병합 컨테이너 `[u32 개수][ (u32 크기)(내용) ]*` 의 n번째 — `netitem.rs` 의
/// 같은 이름 함수와 같은 형식이다(`getMergedGbmData`, 0x87a10).
fn merged_sub(file: &[u8], n: usize) -> Option<&[u8]> {
    let count = u32::from_le_bytes(file.get(..4)?.try_into().ok()?) as usize;
    if n >= count {
        return None;
    }
    let mut pos = 4usize;
    for i in 0..=n {
        let size = u32::from_le_bytes(file.get(pos..pos + 4)?.try_into().ok()?) as usize;
        if i == n {
            return file.get(pos + 4..pos + 4 + size);
        }
        pos = pos.checked_add(4 + size)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped() -> Recipes {
        let vfs = std::fs::read("../payload/data.vfs.mp3").expect("payload/ is committed");
        let index = VfsIndex::build(&vfs, 0, vfs.len() as u64);
        Recipes::build(&vfs, &index)
    }

    /// 출고 데이터에서 나오는 차집합은 정확히 열 개이고 sub 100..109 다.
    ///
    /// 이 테스트가 지키는 것은 「열 개」라는 숫자가 아니라 **두 파일을 맞대는
    /// 방식이 실제로 맞물린다**는 것이다. 어느 한쪽 파싱이 어긋나면 차집합은
    /// 열 개가 아니라 110개나 0개가 된다.
    #[test]
    fn the_unsold_recipes_are_the_ten_the_item_db_has_and_the_shop_does_not() {
        let r = shipped();
        let subs: Vec<u8> = r.rows().iter().map(|x| x.sub).collect();
        assert_eq!(subs, (100u8..110).collect::<Vec<_>>());
    }

    /// 그리고 그 상점이 정말 100줄이라는 것 — 「덧붙이기」가 불가능한 이유가
    /// 이 숫자 하나다. 여기가 99 로 바뀌면 이 기능 전체가 필요 없어진다.
    #[test]
    fn the_shipped_recipe_shop_fills_the_merchant_array_exactly() {
        let vfs = std::fs::read("../payload/data.vfs.mp3").expect("payload/ is committed");
        let index = VfsIndex::build(&vfs, 0, vfs.len() as u64);
        let sold = sold_subs(&vfs, &index).expect("the shipped shop parses");
        assert_eq!(sold.len(), 100, "_merchant holds exactly 100 rows");
    }

    /// 이름과 설명이 칸에 들어가고, 비어 있지 않고, 색 이스케이프가 떨어졌다.
    #[test]
    fn every_row_fits_the_merchant_slots_it_will_be_written_into() {
        for row in shipped().rows() {
            assert!(!row.name.is_empty() && row.name.len() < NAME_MAX, "sub {}", row.sub);
            assert!(!row.desc.is_empty() && row.desc.len() < DESC_MAX, "sub {}", row.sub);
            assert_ne!(row.name.first(), Some(&b'|'), "sub {} kept its colour escape", row.sub);
            // 이름에 `%` 는 없어야 정상이다 — 출고 상점 줄 100개 중 하나도
            // 없다. 있으면 우리가 엉뚱한 레코드를 읽고 있다는 뜻이다.
            // (**설명은 다르다.** 출고 설명에 `100%`·`5%~10%` 가 실제로 있고,
            // 상점 정보 패널은 설명을 포맷으로 넘기지 않는다.)
            assert!(!row.name.contains(&b'%'), "sub {}", row.sub);
        }
    }

    /// 파일이 없으면 빈 목록 — 다섯째 줄이 설치되지 않는다.
    #[test]
    fn a_missing_archive_yields_no_rows_rather_than_a_broken_shop() {
        let empty: Vec<u8> = Vec::new();
        let index = VfsIndex::build(&empty, 0, 0);
        assert!(Recipes::build(&empty, &index).is_empty());
    }
}
