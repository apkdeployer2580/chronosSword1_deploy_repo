import sys
from elftools.elf.elffile import ELFFile
e=ELFFile(open(sys.argv[1],'rb'))
st=e.get_section_by_name('.symtab')
funcs=[];objs=[]
for s in st.iter_symbols():
    t=s['st_info']['type']
    if t=='STT_FUNC' and s['st_shndx']!='SHN_UNDEF': funcs.append((s['st_value'],s['st_size'],s.name))
    elif t=='STT_OBJECT' and s['st_shndx']!='SHN_UNDEF': objs.append((s['st_value'],s['st_size'],s.name))
funcs.sort(); objs.sort()
print(f"FUNCS {len(funcs)}  OBJS {len(objs)}")
with open(sys.argv[2],'w',encoding='utf-8') as f:
    for v,sz,n in funcs: f.write(f"{v:#010x} {sz:#8x} {n}\n")
with open(sys.argv[3],'w',encoding='utf-8') as f:
    for v,sz,n in objs: f.write(f"{v:#010x} {sz:#8x} {n}\n")
# print a sample spread
for i in range(0,len(funcs), max(1,len(funcs)//60)):
    v,sz,n=funcs[i]; print(f"  {v:#010x} {sz:#6x} {n}")
