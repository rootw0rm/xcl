"""Find the D3D12.X inline command-list record writers in XboxBvhBuilder.dll (objdump AT&T
listing): `mov 0x18(%L),%P ; cmp 0x30|0x28(%L),%P ; jae fallback` … `add $N,%P ; mov %P,0x18(%L)`.
Per site: list register, bound field, record length, header (immediate or list field) and
the vtable slot of the fallback call. Branch targets may have or omit the 0x prefix.

usage: objdump -d --no-show-raw-insn XboxBvhBuilder.dll > bvhb.asm; python record_writers.py bvhb.asm"""
import re, collections, sys

lines = open(sys.argv[1], encoding='utf-8', errors='replace').read().splitlines()
ins = []
for l in lines:
    m = re.match(r'\s+([0-9a-f]+):\s+(.*)$', l)
    if m:
        ins.append((int(m.group(1), 16), m.group(2).strip()))
idx = {a: i for i, (a, _) in enumerate(ins)}
control_prefixes = re.compile(r'^(?:(?:rep|repe|repz|repne|repnz|bnd|notrack|data16|addr32|cs|ds)\s+)+')
sites = []
for i, (a, t) in enumerate(ins):
    m = re.match(r'mov\s+0x18\(%(\w+)\),%(\w+)$', t)
    if not m:
        continue
    L, P = m.groups()
    if i + 2 >= len(ins):
        continue
    m2 = re.match(rf'cmp\s+0x(30|28)\(%{L}\),%{P}$', ins[i + 1][1])
    m3 = re.match(r'jae\s+(?:0x)?([0-9a-f]+)', ins[i + 2][1])
    if not (m2 and m3):
        continue
    bound, fallback = m2.group(1), int(m3.group(1), 16)
    body = []
    length = None
    for j in range(i + 3, min(i + 60, len(ins))):
        tj = ins[j][1]
        body.append(tj)
        ma = re.match(rf'(?:add\s+\$0x([0-9a-f]+),%{P}|lea\s+0x([0-9a-f]+)\(%(\w+)\),%\w+)$', tj)
        if ma and ma.group(1):
            length = int(ma.group(1), 16)
        if re.match(rf'mov\s+%\w+,0x18\(%{L}\)$', tj):
            if length is None:
                # lea 0xN(%rcx),%rax style
                for tb in body:
                    ml = re.match(r'lea\s+0x([0-9a-f]+)\(%\w+\),%\w+$', tb)
                    if ml:
                        length = int(ml.group(1), 16)
            break
    # header: first store to (%P) / 0x0(%P) – immediate or from a list field
    header = '?'
    for tb in body:
        mh = re.match(rf'movl\s+\$0x([0-9a-f]+),\(%{P}\)$', tb)
        if mh:
            header = 'imm ' + mh.group(1)
            break
        mf = re.match(rf'mov\s+0x([0-9a-f]+)\(%{L}\),%(\w+)$', tb)
        if mf:
            header = f'list+{mf.group(1)}'
            break
    # fallback: first reachable indirect call from the jae target
    slot = '?'
    k = idx.get(fallback)
    if k is not None:
        pending = collections.deque([k])
        visited = set()
        while pending and len(visited) < 12:
            j = pending.popleft()
            if j in visited or j >= len(ins):
                continue
            visited.add(j)
            tj = control_prefixes.sub('', ins[j][1])
            mc = re.match(r'call\s+\*0x([0-9a-f]+)\(%\w+\)$', tj)
            if mc:
                slot = int(mc.group(1), 16) // 8
                break
            mnemonic = tj.split(maxsplit=1)[0] if tj else ''
            if mnemonic in ('ret', 'retq', 'ud2'):
                continue
            if mnemonic not in ('jmp', 'jmpq'):
                pending.append(j + 1)
            if mnemonic.startswith(('j', 'loop')):
                branch = re.match(r'\w+\s+(?:0x)?([0-9a-f]+)(?:\s|$)', tj)
                target = idx.get(int(branch.group(1), 16)) if branch else None
                if target is not None:
                    pending.append(target)
    sites.append((a, L, bound, length, header, slot))

print(f'{len(sites)} record write sites')
kinds = collections.Counter((h, ln, s, b) for _, _, b, ln, h, s in sites)
for (h, ln, s, b), n in kinds.most_common():
    print(f'  {n:3}× header {h:18} length {ln} B  bound +0x{b}  fallback slot {s}')
