//! 네트워크 전용 장비를 싱글 게임에 돌려주는 두 패치의 데이터 쪽 절반.
//!
//! 장비 종류마다 아이템 목록의 꼬리가 "네트워크 전용"이다 — 온라인 모드가
//! 살아 있던 시절 서버 콘텐츠로만 나오던 영웅/전설/신화/초월 장비들. 무엇이
//! 그것을 막는지는 전부 실측했다:
//!
//! * **장착**: 아이템 템플릿의 **바이트 8** (부호 있는)이 플래그다. 일반
//!   아이템은 -1, 네트워크 아이템은 0..4. 여섯 `Check_*` 장착 검사기 전부와
//!   `GameMenuFrame_KeyPressed+0x9f74`(목걸이·반지 장착 액션)가
//!   `ldrsb rX,[item,#8]; cmp rX,#0; bge/blt` 로 이 바이트 하나만 보고
//!   -1(장착 불가)을 답한다. 코드 쪽 게이트는 그 일곱이 전부다 —
//!   `patch.rs::NET_ITEM_SITES`.
//! * **드랍**: 코드 게이트가 **없다**. 몬스터 드랍은 전적으로 데이터다:
//!   `EMY/M/*`·`EMY/HM/*`(DES, `des.rs` 의 그 변형)의 몬스터 레코드마다
//!   드랍 슬롯 12개(`+0x192` 종류, `+0x1aa` 인덱스, `+0x1c4` 확률)가 있고,
//!   출고 데이터 60파일 전체를 복호화해 보면 장비 드랍 500종의 최대
//!   인덱스가 모든 종류에서 정확히 "바이트 8 이 -1 인 마지막 아이템"이다.
//!   네트워크 아이템을 참조하는 드랍 엔트리는 0개.
//!
//! 그래서 드랍 쪽도 데이터로 푼다: 몬스터가 로드될 때
//! (`patch.rs::host_net_item_drop`) 그 몬스터가 이미 장비를 드랍하는
//! 종류에서 네트워크 아이템 하나를 골라 빈 드랍 슬롯에 끼워 넣는다.
//! 확률 판정·드랍 오브젝트·오토루팅·픽업·아이템 생성은 전부 게스트
//! 코드가 원래 하던 그대로다.
//!
//! 이 모듈은 그 "무엇이 네트워크 아이템인가"를 게임 자신의 데이터에서
//! 읽는다. 하드코딩하지 않는 이유는 DES 테이블과 같다 — 표가 데이터에
//! 있는데 옮겨 적으면 옮겨 적은 쪽이 틀린다. 실제로 커뮤니티 표(xlsx)는
//! 샷건 18..20 을 일반 아이템으로 적었지만 템플릿 바이트 8 은 0/0/1 이고,
//! 게임의 장착 검사와 드랍 데이터(샷건 드랍 최대 인덱스 17)는 템플릿
//! 쪽이 맞다고 말한다.
//!
//! # `ITEM/%03d` 의 형식
//!
//! `ITEM_Load`(0xf1ff8)가 읽는 그대로: 파일은 병합 컨테이너
//! `[u32 개수][ (u32 크기)(내용) ]*개수` (LE, `getMergedGbmData` 0x87a10),
//! 종류 k 는 파일 `ITEM/%03d` % (k/10) 의 서브블롭 k%10 이다. 서브블롭은
//! `[u16 레코드수][50바이트 레코드들]` — 레코드 크기 50 은 `ITEM_getOffset`
//! (0xf0804)의 스킵 시퀀스(2+0xd+16*2+3)에서 나오고, 출고 파일 크기가
//! 전부 `2 + n*50` 으로 떨어진다.

use crate::files::{vfs_hash, VfsIndex};

/// 장비 종류들 — 드랍 테이블과 장착 검사가 아는 22개.
/// 0..18 무기·방어구, 22 목걸이, 23 반지, 48 머신건. (19..21 은 빈 종류,
/// 0x16/0x17 겹침 없음 — `ITEM/ITEM_LAST_IDX` 가 19..21 에 0 을 적는다.)
const EQUIP_KINDS: [u8; 22] =
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 22, 23, 48];

/// 레코드 크기와 그 안의 플래그 위치. 인스턴스의 `item[8]` 은
/// `ITEM_Load` 가 이 바이트를 그대로 복사한 것이다.
const ITEM_REC: usize = 50;
const ITEM_REC_NETFLAG: usize = 8;

/// 레코드 안의 겉모습 두 필드. `ITEM_Load`(0xf1ff8)가 `item[k] = record[k]`
/// 로 통째 복사하므로 살아 있는 아이템의 `item[1]`·`item[2..3]` 과 같은
/// 자리다 — 케이브의 외형 기록(`cave/src/skin.c`)이 베껴 두는 그 둘.
/// `item[2..3]` 은 `UM_ByteToInt16`, 즉 **리틀엔디안**이다(같은 바이너리의
/// `UM_ByteToShort_BIG` 와 헷갈리지 말 것).
const ITEM_REC_SPRITE: usize = 1;
const ITEM_REC_PAL: usize = 2;

/// 종류별 네트워크 아이템 인덱스 집합, 게임 데이터에서 한 번 읽은 것.
pub struct NetItems {
    /// `(종류, 그 종류의 네트워크 인덱스들)`. 인덱스는 최대 109 라 u8.
    sets: Vec<(u8, Vec<u8>)>,
    /// 드랍 주입용 LCG. 고정 시드 — 프로브 재생에서 몬스터 로드 순서가
    /// 같으면 주입도 같아, 결정적 재생이 유지된다.
    rng: u32,
}

impl Default for NetItems {
    fn default() -> Self {
        Self { sets: Vec::new(), rng: 0x9e37_79b9 }
    }
}

impl NetItems {
    /// 아카이브에서 22개 장비 종류의 템플릿을 읽어 집합을 만든다.
    /// 형식이 어긋나는 종류는 조용히 건너뛴다 — 빈 집합은 그 종류를
    /// 주입 대상에서 빼는 것 이상을 하지 않는다.
    pub fn build(vfs: &[u8], index: &VfsIndex) -> Self {
        let mut out = Self::default();
        for &kind in &EQUIP_KINDS {
            let name = format!("ITEM/{:03}", kind / 10);
            let Some((off, len)) = index.get(vfs_hash(name.as_bytes())) else { continue };
            let Some(file) = vfs.get(off as usize..(off + len) as usize) else { continue };
            let Some(blob) = merged_sub(file, (kind % 10) as usize) else { continue };
            if blob.len() < 2 {
                continue;
            }
            let n = u16::from_le_bytes([blob[0], blob[1]]) as usize;
            if blob.len() < 2 + n * ITEM_REC {
                continue;
            }
            let idxs: Vec<u8> = (0..n.min(0x100))
                .filter(|&i| (blob[2 + i * ITEM_REC + ITEM_REC_NETFLAG] as i8) >= 0)
                .map(|i| i as u8)
                .collect();
            if !idxs.is_empty() {
                out.sets.push((kind, idxs));
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

    /// 전체 네트워크 아이템 수 — 부팅 로그용.
    pub fn total(&self) -> usize {
        self.sets.iter().map(|(_, v)| v.len()).sum()
    }

    pub fn kinds(&self) -> usize {
        self.sets.len()
    }

    fn set(&self, kind: u8) -> Option<&[u8]> {
        self.sets.iter().find(|(k, _)| *k == kind).map(|(_, v)| v.as_slice())
    }

    pub fn has(&self, kind: u8) -> bool {
        self.set(kind).is_some()
    }

    /// `kinds` 중 하나를 고르고 그 종류의 네트워크 인덱스 하나를 고른다.
    /// `kinds` 는 호출자가 `has` 로 거른 것이라 비어 있지 않으면 성공한다.
    pub fn roll(&mut self, kinds: &[u8]) -> Option<(u8, u8)> {
        if kinds.is_empty() {
            return None;
        }
        let kind = kinds[self.next(kinds.len())];
        let len = self.set(kind)?.len();
        let pick = self.next(len);
        let idx = self.set(kind)?[pick];
        Some((kind, idx))
    }

    /// `[0, n)`. Numerical Recipes 상수의 LCG — 품질보다 재현성이 목적이다.
    fn next(&mut self, n: usize) -> usize {
        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.rng >> 16) as usize) % n.max(1)
    }
}

/// 병합 컨테이너의 n 번째 서브블롭. `getMergedGbmData` 가 걷는 그대로
/// `[u32 개수][ (u32 크기)(내용) ]*` 를 걷는다.
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

/// 한 템플릿의 겉모습 `(스프라이트, 팔레트)`. `kind_with_look` 의 역방향이고,
/// 그쪽 조회가 공허하지 않음을 보이는 테스트가 이것으로 정답을 만든다.
pub fn template_look(vfs: &[u8], index: &VfsIndex, kind: u8, idx: usize) -> Option<(u8, i16)> {
    let name = format!("ITEM/{:03}", kind / 10);
    let (off, len) = index.get(vfs_hash(name.as_bytes()))?;
    let file = vfs.get(off as usize..(off + len) as usize)?;
    let blob = merged_sub(file, (kind % 10) as usize)?;
    let n = u16::from_le_bytes([*blob.first()?, *blob.get(1)?]) as usize;
    if idx >= n {
        return None;
    }
    let r = blob.get(2 + idx * ITEM_REC..2 + (idx + 1) * ITEM_REC)?;
    Some((r[ITEM_REC_SPRITE], i16::from_le_bytes([r[ITEM_REC_PAL], r[ITEM_REC_PAL + 1]])))
}

/// 이 겉모습을 가진 템플릿의 **종류와 인덱스**, `kinds` 안에서.
///
/// 외형 기록은 스프라이트 로더가 필요로 하는 것만 들고 있고(`item[1]` 과
/// `item[2..3]`) 장비 칸의 아이콘은 `getImage(*_imgITMIcon, item[0x96])`,
/// 즉 **종류로** 고른다. 그래서 기록만으로는 아이콘을 못 고르고 종류를
/// 되찾아야 한다. 되찾을 수 있는 이유는 템플릿과 인스턴스가 같은 바이트
/// 배열이기 때문이다 — 위 `ITEM_REC_SPRITE`/`ITEM_REC_PAL` 참고. 인덱스까지
/// 돌려주는 것은 정보 패널이 `ITEM_Load(종류, 인덱스, 버퍼)` 로 그 장비를
/// 통째로 되살려 쓰기 때문이다.
///
/// 여러 종류가 같은 겉모습을 내면 `kinds` 순서로 처음 것을 답한다. 그것이
/// 위험하지 않은 이유는 호출자가 넘기는 `kinds` 가 **외형 교체가 허용된
/// 그 묶음**이라서다 — 무기·방패는 한 종류뿐이고, 방어구는 같은 부위의 셋뿐.
pub fn kind_with_look(
    vfs: &[u8],
    index: &VfsIndex,
    kinds: &[u8],
    sprite: u8,
    pal: i16,
) -> Option<(u8, u8)> {
    for &kind in kinds {
        let name = format!("ITEM/{:03}", kind / 10);
        let Some((off, len)) = index.get(vfs_hash(name.as_bytes())) else { continue };
        let Some(file) = vfs.get(off as usize..(off + len) as usize) else { continue };
        let Some(blob) = merged_sub(file, (kind % 10) as usize) else { continue };
        if blob.len() < 2 {
            continue;
        }
        let n = u16::from_le_bytes([blob[0], blob[1]]) as usize;
        if blob.len() < 2 + n * ITEM_REC {
            continue;
        }
        for i in 0..n {
            let r = &blob[2 + i * ITEM_REC..2 + (i + 1) * ITEM_REC];
            let p = i16::from_le_bytes([r[ITEM_REC_PAL], r[ITEM_REC_PAL + 1]]);
            if r[ITEM_REC_SPRITE] == sprite && p == pal {
                return Some((kind, i as u8));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_archive_yields_the_known_boundaries() {
        let vfs = std::fs::read("../payload/data.vfs.mp3").expect("payload/ is committed");
        let index = VfsIndex::build(&vfs, 0, vfs.len() as u64);
        let items = NetItems::build(&vfs, &index);
        assert_eq!(items.kinds(), EQUIP_KINDS.len(), "a kind failed to parse");
        // 실측 경계 몇 개: 둔기 36..=72, 반지 39..=89, 머신건 18..=36.
        for (kind, lo, hi) in [(0u8, 36u8, 72u8), (23, 39, 89), (48, 18, 36)] {
            let set = items.set(kind).unwrap();
            assert_eq!(set.first(), Some(&lo), "kind {kind}");
            assert_eq!(set.last(), Some(&hi), "kind {kind}");
            assert_eq!(set.len(), (hi - lo + 1) as usize, "kind {kind} is contiguous");
        }
        // 샷건의 별종 셋: 커뮤니티 표는 일반이라 적지만 템플릿 바이트 8 은
        // 0/0/1 이고, 출고 드랍 데이터의 샷건 최대 인덱스도 17 이다.
        let shotgun = items.set(3).unwrap();
        assert_eq!(shotgun.first(), Some(&18));
        // 전체 수: 커뮤니티 표의 625 에서, 데이터에 없는 유령 행(둔기
        // dcode 73, 표에 이름도 없다)을 빼고 샷건 별종 셋을 더한 것.
        assert_eq!(items.total(), 627);
    }

    #[test]
    fn roll_stays_inside_the_asked_kinds() {
        let vfs = std::fs::read("../payload/data.vfs.mp3").expect("payload/ is committed");
        let index = VfsIndex::build(&vfs, 0, vfs.len() as u64);
        let mut items = NetItems::build(&vfs, &index);
        for _ in 0..100 {
            let (kind, idx) = items.roll(&[0, 23]).unwrap();
            assert!(kind == 0 || kind == 23);
            let set = items.set(kind).unwrap();
            assert!(set.contains(&idx));
        }
        assert!(items.roll(&[]).is_none());
    }
}
