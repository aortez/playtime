import json,re,sys

def resolve_ranges(header,size):
    if type(size) is not int or size<0 or (header is not None and not isinstance(header,str)): raise ValueError('invalid argument')
    full={'status':200,'ranges':[[0,size-1]] if size else []}
    if header is None: return full
    h=header.strip(' \t')
    if not h or not h.lower().startswith('bytes='): return full
    parts=h[6:].split(',')
    if len(parts)>32: return full
    spans=[]
    for part in parts:
        m=re.fullmatch(r'([0-9]*)-([0-9]*)',part.strip(' \t'))
        if not m or not any(m.groups()): return full
        a,b=m.groups()
        if a:
            start=int(a);end=int(b) if b else size-1
            if b and start>end: return full
            if start<size: spans.append((start,min(end,size-1)))
        elif int(b)>0 and size>0: spans.append((max(0,size-int(b)),size-1))
    merged=[]
    for a,b in sorted(spans):
        if merged and a<=merged[-1][1]+1: merged[-1][1]=max(merged[-1][1],b)
        else: merged.append([a,b])
    return {'status':206 if merged else 416,'ranges':merged}

if __name__=='__main__':
    if len(sys.argv)!=3 or not re.fullmatch('[0-9]+',sys.argv[1]):
        print('usage: ranges.py SIZE HEADER',file=sys.stderr);sys.exit(2)
    print(json.dumps(resolve_ranges(sys.argv[2],int(sys.argv[1]))))
