import json,sys

def plan(tasks):
    def name(v): return isinstance(v,str) and bool(v) and v==v.strip()
    if not isinstance(tasks,list): raise ValueError('expected a list')
    graph={}
    for t in tasks:
        if not isinstance(t,dict) or not name(t.get('id')) or not isinstance(t.get('deps'),list) or not all(name(x) for x in t['deps']): raise ValueError('invalid task')
        if t['id'] in graph: raise ValueError('duplicate id')
        graph[t['id']]=set(t['deps'])
    if any(d not in graph for deps in graph.values() for d in deps): raise ValueError('unknown dependency')
    done=set();waves=[]
    while len(done)!=len(graph):
        ready=sorted(k for k,v in graph.items() if k not in done and v<=done)
        if not ready: raise ValueError('cycle: '+', '.join(sorted(set(graph)-done)))
        waves.append(ready);done.update(ready)
    return waves

if __name__=='__main__':
    try:
        with open(sys.argv[1]) as f: result=plan(json.load(f))
    except (OSError,ValueError,IndexError) as e:
        print(str(e),file=sys.stderr);sys.exit(2)
    with open(sys.argv[2],'w') as f: json.dump(result,f)
