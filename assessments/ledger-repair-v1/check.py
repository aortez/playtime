from assessment_support import emit_report, UnavailableModule
"""Independent acceptance checks. Run only inside the locked workshop image."""
import json
from pathlib import Path
import subprocess
import tempfile

HEADER = 'transaction_id,customer,amount,status\n'


def expected(accepted=0, voided=0, duplicates=0, rejected=0, customers=()):
    return {'accepted_rows': accepted, 'voided_rows': voided,
            'duplicate_rows': duplicates, 'rejected_rows': rejected,
            'total_cents': sum(v for _, v in customers),
            'customers': [{'customer': k, 'total_cents': v} for k, v in customers]}


CASES = [
    ('sample', HEADER + 't1,Ada,10.01,settled\nt2,Bea,3.00,settled\nt1,Ada,10.01,settled\nt3,Ada,-4.00,settled\nt4,Bea,99.00,void\nt5,Ada,1.001,settled\n', expected(3,1,1,1,[('Ada',601),('Bea',300)])),
    ('exact_cents', HEADER + 'a,X,0.29,settled\nb,X,0.58,settled\nc,X,-0.07,settled\nd,Y,+90071992547409.91,settled\ne,Y,0.01,settled\n', expected(5,customers=[('X',80),('Y',9007199254740992)])),
    ('validate_before_dedup', HEADER + 'a,Ann,nan,settled\na,Ann,1.00,settled\na,Ann,2.00,settled\na,Ann,3.001,settled\nb,Bob,8.00,void\nb,Bob,8.00,settled\nc,Ann,-1.00,settled\n', expected(2,1,2,2,[('Ann',0)])),
    ('invalid_rows', HEADER + '1,A,NaN,settled\n2,A,Infinity,settled\n3,A,1e2,settled\n4,A,.5,settled\n5,A,5.,settled\n6,A,5.001,settled\n7,A,2,SETTLED\n8,,2,settled\n,A,2,settled\n9,A,2,settled,extra\n10,A,2\n11,A,１２,settled\n12,A,1_000,settled\n13,A,--2,settled\n', expected(rejected=14)),
    ('quoted_unicode_sorted', HEADER + ' z ,Zoë, 01.2 , settled \na,"Li, Ana",+2,settled\nb,Áda,-0.2,settled\nc,Zoë,-1.20,settled\nd,Only Void,3,void\n\n', expected(4,1,customers=[('Li, Ana',200),('Zoë',0),('Áda',-20)])),
    ('empty_ledger', HEADER+'\n\n', expected()),
]
results=[]
program=Path('/candidate/report.py')
with tempfile.TemporaryDirectory() as tmp:
    tmp=Path(tmp)
    def invoke(name, content):
        if not program.is_file():
            raise FileNotFoundError("missing report.py")
        source=tmp/(name+'.csv')
        output=tmp/(name+'.json')
        if content is not None:
            source.write_text(content,encoding='utf-8')
        result=subprocess.run(['python3',str(program),str(source),str(output)],cwd='/candidate',capture_output=True,timeout=8)
        return result,output
    for name,content,want in CASES:
        try:
            result,output=invoke(name,content)
            got=json.loads(output.read_text()) if output.is_file() else None
            counts=['accepted_rows','voided_rows','duplicate_rows','rejected_rows','total_cents']
            integer_types=isinstance(got,dict) and all(type(got.get(k)) is int for k in counts) and all(type(c.get('total_cents')) is int for c in got.get('customers',[]))
            passed=result.returncode==0 and got==want and integer_types
            results.append({'case':name,'passed':passed,'exit_code':result.returncode,'actual':got,'expected':want,'stderr':result.stderr.decode(errors='replace')[:2000]})
        except BaseException as exc:
            results.append({'case':name,'passed':False,'error':str(exc)[:2000]})
    for name,content in [('wrong_header','customer,transaction_id,amount,status\n'),('missing_input',None)]:
        try:
            result,output=invoke(name,content)
            results.append({'case':name,'passed':result.returncode==2 and bool(result.stderr.strip()) and not output.exists(),'exit_code':result.returncode,'stderr':result.stderr.decode(errors='replace')[:2000]})
        except BaseException as exc:
            results.append({'case':name,'passed':False,'error':str(exc)[:2000]})
try:
    got=json.loads(Path('/candidate/report.json').read_text())
    results.append({'case':'delivered_report','passed':got==CASES[0][2],'actual':got})
except BaseException as exc:
    results.append({'case':'delivered_report','passed':False,'error':str(exc)})
readme=Path('/candidate/README.md')
results.append({'case':'usage_and_validation_notes_present','passed':readme.is_file() and readme.stat().st_size>50})
emit_report({'passed':sum(r['passed'] for r in results),'total':len(results),'results':results})
raise SystemExit(0 if all(r['passed'] for r in results) else 1)
