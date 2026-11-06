import importlib.util,json,subprocess
from pathlib import Path
results=[]
def check(name,fn):
    try:fn();results.append({'case':name,'passed':True})
    except Exception as e:results.append({'case':name,'passed':False,'error':str(e)[:1500]})
def equal(a,b):assert a==b,(a,b)
spec=importlib.util.spec_from_file_location('candidate','/candidate/ranges.py');module=importlib.util.module_from_spec(spec)
try:spec.loader.exec_module(module)
except Exception as e:
    print(json.dumps({'passed':0,'total':37,'failure':'import: '+str(e)}));raise SystemExit(1)
def run(header,size,status,ranges):
    got=module.resolve_ranges(header,size)
    equal(got,{'status':status,'ranges':ranges})
    assert type(got['status']) is int and all(type(v) is int for r in got['ranges'] for v in r)
cases=[
 ('none',None,10,200,[[0,9]]),('empty',' \t',10,200,[[0,9]]),('zero_no_header',None,0,200,[]),
 ('explicit','bytes=2-4',10,206,[[2,4]]),('clipping','bytes=8-100',10,206,[[8,9]]),('open','bytes=8-',10,206,[[8,9]]),
 ('suffix','bytes=-2',10,206,[[8,9]]),('long_suffix','bytes=-999999999999999999999999',10,206,[[0,9]]),
 ('mixed_sorted','bytes=-2,0-1',10,206,[[0,1],[8,9]]),('adjacent_overlap','bytes=6-8,0-3,2-5',10,206,[[0,8]]),
 ('case_ows',' \tBYTES=002-004,\t8-9 \t',10,206,[[2,4],[8,9]]),('unknown_unit','items=0-2',10,200,[[0,9]]),
 ('skip_unsatisfiable','bytes=99-100,3-4,-0',10,206,[[3,4]]),('all_unsatisfiable','bytes=99-100,-0',10,416,[]),
 ('zero_valid','bytes=0-0,-1',0,416,[]),('large_start','bytes=999999999999999999999999-',10,416,[]),
 ('reversed','bytes=3-2,5-8',10,200,[[0,9]]),('internal_space','bytes=0 -2',10,200,[[0,9]]),
 ('separator_space','bytes =0-2',10,200,[[0,9]]),('unicode_digits','bytes=１-２',10,200,[[0,9]]),
 ('empty_member','bytes=0-2,,5-6',10,200,[[0,9]]),('extra_hyphen','bytes=0-2-3',10,200,[[0,9]]),
 ('plus','bytes=+0-2',10,200,[[0,9]]),('bad_member_invalidates_all','bytes=0-2,junk',10,200,[[0,9]]),
 ('limit32','bytes='+','.join(['0-0']*32),10,206,[[0,0]]),('limit33','bytes='+','.join(['0-0']*33),10,200,[[0,9]])]
for name,h,s,status,ranges in cases:check(name,lambda h=h,s=s,status=status,ranges=ranges:run(h,s,status,ranges))
def invalid(header,size):
    try:module.resolve_ranges(header,size)
    except ValueError as e:assert str(e)
    else:raise AssertionError('expected ValueError')
for name,h,s in [('bool_size',None,True),('negative_size',None,-1),('float_size',None,1.5),('bad_header',10,10)]:check(name,lambda h=h,s=s:invalid(h,s))
def cli(args,want=None):
    p=subprocess.run(['python3','/candidate/ranges.py',*args],capture_output=True,text=True,timeout=10)
    if want is None:equal(p.returncode,2);assert p.stderr.strip()
    else:equal(p.returncode,0);equal(json.loads(p.stdout),want)
check('cli_valid',lambda:cli(['10','bytes=-2,0-1'],{'status':206,'ranges':[[0,1],[8,9]]}))
check('cli_unsatisfiable',lambda:cli(['0','bytes=0-1'],{'status':416,'ranges':[]}))
check('cli_invalid_size',lambda:cli(['-1','bytes=0-1']))
check('cli_unicode_size',lambda:cli(['１２','bytes=0-1']))
check('cli_missing_args',lambda:cli([]))
check('delivered_example',lambda:equal(json.loads(Path('/candidate/example.json').read_text()),{'status':206,'ranges':[[0,1],[8,9]]}))
def readme():assert Path('/candidate/README.md').stat().st_size>50
check('readme',readme)
print(json.dumps({'passed':sum(x['passed'] for x in results),'total':len(results),'results':results},indent=2))
raise SystemExit(0 if all(x['passed'] for x in results) else 1)
