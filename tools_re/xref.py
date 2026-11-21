import sys, bisect
from elftools.elf.elffile import ELFFile
from capstone import *
p=sys.argv[1]
e=ELFFile(open(p,'rb'))
t=e.get_section_by_name('.text'); d=t.data(); base=t['sh_addr']
st=e.get_section_by_name('.symtab')
funcs=sorted([(s['st_value']&~1,s.name) for s in st.iter_symbols() if s['st_info']['type']=='STT_FUNC' and s['st_shndx']!='SHN_UNDEF'])
addrs=[f[0] for f in funcs]
def owner(a):
    i=bisect.bisect_right(addrs,a)-1
    return funcs[i][1] if i>=0 else '?'
rel=e.get_section_by_name('.rel.plt'); dsym=e.get_section_by_name('.dynsym')
plt=e.get_section_by_name('.plt'); pb=plt['sh_addr']
names={}
for i,r in enumerate(rel.iter_relocations()):
    names[pb+20+12*i]=dsym.get_symbol(r['r_info_sym']).name
want=set(sys.argv[2:])
md=Cs(CS_ARCH_ARM, CS_MODE_ARM)
off=0; hits={w:[] for w in want}
while off<len(d):
    got=0
    for i in md.disasm(d[off:], base+off, count=300000):
        got+=1; off=i.address-base+4
        if i.mnemonic.startswith('b') and i.op_str.startswith('#'):
            try: tt=int(i.op_str[1:],0)
            except: continue
            n=names.get(tt)
            if n in want: hits[n].append((i.address, owner(i.address)))
    if got==0: off+=4
for w in sys.argv[2:]:
    print(f"--- {w}: {len(hits[w])} call sites")
    for a,o in hits[w][:12]: print(f"    {a:#x}  {o}")
