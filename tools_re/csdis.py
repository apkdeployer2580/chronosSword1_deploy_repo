import sys, bisect
from elftools.elf.elffile import ELFFile
from capstone import *
PATH = sys.argv[1] if len(sys.argv)>1 else None

class Bin:
    def __init__(self, path):
        self.f=open(path,'rb'); self.e=ELFFile(self.f)
        self.data=open(path,'rb').read()
        self.segs=[(s['p_vaddr'],s['p_offset'],s['p_filesz'],s['p_memsz']) for s in self.e.iter_segments() if s['p_type']=='PT_LOAD']
        self.syms={}; self.byaddr=[]
        for secn in ('.symtab','.dynsym'):
            sec=self.e.get_section_by_name(secn)
            if not sec: continue
            for s in sec.iter_symbols():
                if s['st_shndx']=='SHN_UNDEF' or not s.name: continue
                t=s['st_info']['type']
                if t in ('STT_FUNC','STT_OBJECT'):
                    self.syms.setdefault(s['st_value']&~1,(s.name,s['st_size'],t))
        self.byaddr=sorted(self.syms.keys())
        # PLT: build map plt_addr -> import name
        self.plt={}
        rel=self.e.get_section_by_name('.rel.plt')
        dsym=self.e.get_section_by_name('.dynsym')
        got=[]
        for r in rel.iter_relocations():
            nm=dsym.get_symbol(r['r_info_sym']).name
            got.append((r['r_offset'],nm))
        self.gotmap=dict(got)
        # plt entries: 20 bytes each after 20-byte header in ARM ELF
        pltsec=self.e.get_section_by_name('.plt')
        base=pltsec['sh_addr']; size=pltsec['sh_size']
        # decode each plt entry to find got address
        md=Cs(CS_ARCH_ARM, CS_MODE_ARM)
        off=base+20
        idx=0
        for i,(gotaddr,nm) in enumerate(got):
            self.plt[base+20+12*i]=nm
        self.pltbase=base; self.pltcount=len(got)
    def read(self, va, n):
        for v,o,fs,ms in self.segs:
            if v<=va<v+ms:
                d=va-v
                if d+n<=fs: return self.data[o+d:o+d+n]
                out=bytearray()
                for k in range(n):
                    out.append(self.data[o+d+k] if d+k<fs else 0)
                return bytes(out)
        return None
    def symat(self, va):
        i=bisect.bisect_right(self.byaddr, va)-1
        if i<0: return None
        a=self.byaddr[i]; n,sz,t=self.syms[a]
        if sz and va>=a+sz and va!=a: 
            if va-a>0x2000: return None
        return (n, va-a)
    def label(self, va):
        if va in self.plt: return f"<PLT:{self.plt[va]}>"
        s=self.symat(va)
        if not s: return f"{va:#x}"
        n,d=s
        return n if d==0 else f"{n}+{d:#x}"

def disas(b, start, end, out=sys.stdout):
    md=Cs(CS_ARCH_ARM, CS_MODE_ARM); md.detail=False
    code=b.read(start, end-start)
    for i in md.disasm(code, start):
        extra=""
        if i.mnemonic.startswith(('bl','b')) and i.op_str.startswith('#'):
            try:
                t=int(i.op_str[1:],0); extra="   ; "+b.label(t)
            except: pass
        if 'pc' in i.op_str and '[' in i.op_str:
            # ldr rX, [pc, #imm]
            try:
                import re
                m=re.search(r'\[pc, #(-?\d+)\]', i.op_str)
                if m:
                    t=i.address+8+int(m.group(1))
                    v=int.from_bytes(b.read(t,4),'little')
                    extra=f"   ; ={v:#x} {b.label(v)}"
            except Exception as ex: pass
        print(f"  {i.address:#010x}  {i.mnemonic:8s} {i.op_str}{extra}", file=out)

if __name__=='__main__':
    b=Bin(PATH)
    for name in sys.argv[2:]:
        if name.startswith('@'):
            a=int(name[1:],0); n=0x100
            print(f"\n===== {a:#x} =====")
            disas(b,a,a+n); continue
        # find symbol
        found=[(a,v) for a,v in b.syms.items() if v[0]==name]
        if not found: print(f"!! {name} not found"); continue
        a,(n,sz,t)=found[0]
        print(f"\n===== {n} @ {a:#x} size {sz:#x} =====")
        disas(b,a,a+(sz or 0x80))
