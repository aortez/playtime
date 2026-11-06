from assessment_support import emit_report, UnavailableModule
import copy,importlib.util,json,subprocess,tempfile
from pathlib import Path
results=[]
def check(name,fn):
    try:fn();results.append({'case':name,'passed':True})
    except BaseException as e:results.append({'case':name,'passed':False,'error':str(e)[:1500]})
def equal(a,b):
    assert a==b,(a,b)
spec=importlib.util.spec_from_file_location('candidate','/candidate/planner.py');module=importlib.util.module_from_spec(spec)
try:spec.loader.exec_module(module)
except BaseException as e:
    module=UnavailableModule(e)

def task(i,deps=()):return {'id':i,'deps':list(deps)}
def run(data,want):
    original=copy.deepcopy(data);got=module.plan(data)
    equal(got,want);equal(data,original)
def invalid(data,message=None,not_cycle=False):
    original=copy.deepcopy(data)
    try:module.plan(data)
    except ValueError as e:
        assert str(e)
        if not_cycle: assert not str(e).startswith("cycle:")
        if message is not None:equal(str(e),message)
    else:raise AssertionError('expected ValueError')
    equal(data,original)
check('empty',lambda:run([],[]))
check('independent_sorted',lambda:run([task('z'),task('A'),task('é'),task('a')],[['A','a','z','é']]))
check('wave_snapshot',lambda:run([task('a'),task('b',['a']),task('c',['b'])],[['a'],['b'],['c']]))
check('diamond_reversed',lambda:run([task('d',['c','b']),task('c',['a']),task('b',['a']),task('a')],[['a'],['b','c'],['d']]))
check('duplicate_edges',lambda:run([task('b',['a','a']),task('a')],[['a'],['b']]))
check('extra_fields_no_mutation',lambda:run([{'id':'a','deps':[],'metadata':{'x':[1,2]}}],[['a']]))
check('deep_chain',lambda:run([task(f'n{i:04}',[f'n{i-1:04}'] if i else []) for i in reversed(range(1500))],[[f'n{i:04}'] for i in range(1500)]))
check('cycle_downstream',lambda:invalid([task('z',['b']),task('b',['a']),task('a',['b']),task('free')],'cycle: a, b, z'))
check('self_cycle',lambda:invalid([task('a',['a'])],'cycle: a'))
check('duplicate_ids',lambda:invalid([task('a'),task('a')]))
check('unknown_dependency',lambda:invalid([task('a',['missing'])]))
check('validate_even_if_cycle',lambda:invalid([task('a',['a']),{'id':'b','deps':'a'}],not_cycle=True))
check('invalid_container',lambda:invalid({'id':'a','deps':[]}))
check('invalid_item',lambda:invalid([task('a'),None]))
check('missing_fields',lambda:invalid([{'id':'a'}]))
check('invalid_id',lambda:invalid([task(' a ')]))
check('invalid_dependency',lambda:invalid([task('a',[True])]))
check('empty_id',lambda:invalid([task('')]))
def cli(kind):
    if not Path('/candidate/planner.py').is_file():
        raise FileNotFoundError('missing planner.py')
    with tempfile.TemporaryDirectory() as folder:
        p=Path(folder);source=p/'in.json';target=p/'out.json'
        if kind=='good':source.write_text(json.dumps([task('b',['a']),task('a')]))
        elif kind=='json':source.write_text('{broken')
        elif kind=='cycle':source.write_text(json.dumps([task('a',['a'])]))
        r=subprocess.run(['python3','/candidate/planner.py',str(source),str(target)],capture_output=True,text=True,timeout=10)
        if kind=='good':equal(r.returncode,0);equal(json.loads(target.read_text()),[['a'],['b']])
        else:equal(r.returncode,2);assert r.stderr.strip();assert not target.exists()
for kind in ['good','json','cycle','missing']:check('cli_'+kind,lambda kind=kind:cli(kind))
check('delivered_plan',lambda:equal(json.loads(Path('/candidate/plan.json').read_text()),[['build','docs'],['test'],['ship']]))
def readme():assert Path('/candidate/README.md').stat().st_size>50
check('readme',readme)
emit_report({'passed':sum(x['passed'] for x in results),'total':len(results),'results':results})
raise SystemExit(0 if all(x['passed'] for x in results) else 1)
