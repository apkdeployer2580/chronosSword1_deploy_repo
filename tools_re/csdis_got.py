"""csdis 변형: GOT 간접 전역 접근에 심볼 이름을 단다.

GCC 4.4 PIC 코드는 전역을 `ldr rG, [pc,#lit]` (GOT 오프셋) →
`ldr rP, [r4, rG]` (GOT 슬롯 = 전역 주소) → `ldr/str [rP]` 로 만진다.
이 스크립트는 함수 하나를 선형 추적해서

  * r4 (GOT 베이스) 계산을 흉내내고,
  * 리터럴에서 읽힌 GOT 오프셋이 어느 레지스터/스택 슬롯에 앉는지 따라가서,
  * `[r4, rX]` 로드에 `; GOT -> <심볼>` 주석을 단다.

분기 흐름은 무시하는 선형 근사지만, GCC 가 리터럴을 레지스터에 담아
바로 쓰는 패턴이라 실제로 잘 맞는다. 확신 없는 슬롯은 표시하지 않는다.
"""
import sys, re, bisect
from elftools.elf.elffile import ELFFile
from capstone import *

so = sys.argv[1]
names = sys.argv[2:]

f = open(so, 'rb')
e = ELFFile(f)
data = open(so, 'rb').read()
segs = [(s['p_vaddr'], s['p_offset'], s['p_filesz'], s['p_memsz'])
        for s in e.iter_segments() if s['p_type'] == 'PT_LOAD']

def read(va, n):
    for v, o, fs, ms in segs:
        if v <= va < v + ms:
            d = va - v
            return data[o + d:o + d + n]
    return None

syms = {}
for secn in ('.symtab',):
    sec = e.get_section_by_name(secn)
    for s in sec.iter_symbols():
        if s['st_shndx'] == 'SHN_UNDEF' or not s.name:
            continue
        if s['st_info']['type'] in ('STT_FUNC', 'STT_OBJECT'):
            syms.setdefault(s['st_value'] & ~1, (s.name, s['st_size']))
byaddr = sorted(syms.keys())

def label(va):
    i = bisect.bisect_right(byaddr, va) - 1
    if i < 0:
        return f"{va:#x}"
    a = byaddr[i]
    n, sz = syms[a]
    if va == a:
        return n
    if sz and va < a + sz:
        return f"{n}+{va-a:#x}"
    return f"{va:#x}"

md = Cs(CS_ARCH_ARM, CS_MODE_ARM)
md.detail = False

for name in names:
    found = [(a, v) for a, v in syms.items() if v[0] == name]
    if not found:
        print(f"!! {name} not found")
        continue
    start, (_, size) = found[0]
    end = start + size
    code = read(start, end - start)

    # 선형 추적 상태: 레지스터/스택 슬롯 -> ('lit', GOT오프셋) | ('got', 전역주소)
    val = {}
    got_base = None

    print(f"\n===== {name} @ {start:#x} size {size:#x} =====")
    for i in md.disasm(code, start):
        extra = ""
        m = re.match(r'^(\w+), \[pc, #(-?(?:0x)?[0-9a-f]+)\]$', i.op_str)
        if i.mnemonic == 'ldr' and m:
            t = i.address + 8 + int(m.group(2), 0)
            v = int.from_bytes(read(t, 4), 'little')
            val[m.group(1)] = ('lit', v)
            extra = f"   ; lit={v:#x}"
        elif i.mnemonic == 'add':
            m2 = re.match(r'^(\w+), pc, (\w+)$', i.op_str)
            if m2 and val.get(m2.group(2), (None,))[0] == 'lit':
                got_base = i.address + 8 + val[m2.group(2)][1]
                val[m2.group(1)] = ('gotbase', got_base)
                extra = f"   ; GOT base = {got_base:#x}"
            else:
                m2 = re.match(r'^(\w+), ', i.op_str)
                if m2:
                    val.pop(m2.group(1), None)
        elif i.mnemonic == 'ldr':
            m2 = re.match(r'^(\w+), \[(\w+), (\w+)\]$', i.op_str)
            if m2 and val.get(m2.group(2), (None,))[0] == 'gotbase' \
                  and val.get(m2.group(3), (None,))[0] == 'lit':
                slot = got_base + val[m2.group(3)][1]
                addr = int.from_bytes(read(slot, 4), 'little')
                val[m2.group(1)] = ('got', addr)
                extra = f"   ; GOT -> {label(addr)}"
            else:
                # 스택 저장/복원 추적: ldr rX, [sp,#off]
                m3 = re.match(r'^(\w+), \[sp, #((?:0x)?[0-9a-f]+)\]$', i.op_str)
                if m3 and ('sp', int(m3.group(2), 0)) in val:
                    val[m3.group(1)] = val[('sp', int(m3.group(2), 0))]
                    v = val[m3.group(1)]
                    if v[0] == 'lit':
                        extra = f"   ; lit={v[1]:#x} (spilled)"
                    elif v[0] == 'got':
                        extra = f"   ; GOT -> {label(v[1])} (spilled)"
                else:
                    m4 = re.match(r'^(\w+), \[(\w+)(?:, #((?:0x)?[0-9a-f]+))?\]$', i.op_str)
                    if m4 and val.get(m4.group(2), (None,))[0] == 'got':
                        off = int(m4.group(3), 0) if m4.group(3) else 0
                        extra = f"   ; *{label(val[m4.group(2)][1] + off)}"
                        val.pop(m4.group(1), None)
                    else:
                        m5 = re.match(r'^(\w+), ', i.op_str)
                        if m5:
                            val.pop(m5.group(1), None)
        elif i.mnemonic == 'str':
            m3 = re.match(r'^(\w+), \[sp, #((?:0x)?[0-9a-f]+)\]$', i.op_str)
            if m3 and m3.group(1) in val:
                val[('sp', int(m3.group(2), 0))] = val[m3.group(1)]
            else:
                m4 = re.match(r'^(\w+), \[(\w+)(?:, #((?:0x)?[0-9a-f]+))?\]$', i.op_str)
                if m4 and val.get(m4.group(2), (None,))[0] == 'got':
                    off = int(m4.group(3), 0) if m4.group(3) else 0
                    extra = f"   ; -> *{label(val[m4.group(2)][1] + off)}"
        elif i.mnemonic.startswith('mov'):
            m2 = re.match(r'^(\w+), (\w+)$', i.op_str)
            if m2 and m2.group(2) in val:
                val[m2.group(1)] = val[m2.group(2)]
            else:
                m5 = re.match(r'^(\w+), ', i.op_str)
                if m5:
                    val.pop(m5.group(1), None)
        elif i.mnemonic.startswith(('b', 'bl')) and i.op_str.startswith('#'):
            try:
                t = int(i.op_str[1:], 0)
                extra = "   ; " + label(t)
            except ValueError:
                pass
        else:
            m5 = re.match(r'^(\w+), ', i.op_str)
            if m5 and not i.op_str.startswith(('[',)):
                val.pop(m5.group(1), None)

        print(f"  {i.address:#010x}  {i.mnemonic:8s} {i.op_str}{extra}")
