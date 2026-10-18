"""Independent, deliberately synthetic contract fixture authoring.

Run only when reviewing a contract change. Tests never regenerate expectations.
Uses Python's JSON encoder and hashlib, not the Rust planner or query facade.
This is not an executable worker bundle or a recovery transaction.
"""
from pathlib import Path
import hashlib
import json

ROOT = Path(__file__).parent
RUN = 'recovery-synthetic'
ATTEMPT = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
TIME = '2026-10-18T00:00:00Z'
files = {}

def raw(value):
    return json.dumps(value, separators=(',', ':'), ensure_ascii=False).encode()

def ref(path, data):
    return dict(path=path, sha256=hashlib.sha256(data).hexdigest(), size_bytes=len(data))

def store(path, data):
    if not isinstance(data, bytes):
        data = raw(data)
    files[path] = data
    return ref(path, data)

limits = dict(event_log_bytes=1048576, record_bytes=1048576, batch_bytes=4194304,
              content_bytes=67108864, manifest_bytes=4194304, verified_content_bytes=536870912,
              references=10000, events=100000, page_bytes=4194304)
maintenance = dict(total_bytes=134217728, entries=256, plan_bytes=4194304,
                   inventory_bytes=4194304, selection_bytes=65536, resolution_bytes=65536,
                   inventory_entries=10000, resolution_reserve_bytes=131072, resolution_reserve_entries=4)
scenario = (ROOT.parents[3] / 'playtime-runner/tests/fixtures/known-zero-rejected.json').read_bytes()
configuration = store('inputs/scenario.json', scenario)
workspace = store('inputs/initial-workspace.json', b'{"seed":"synthetic"}\n')
build = dict(rustc_version='synthetic fixture, not executable', target_triple='synthetic-fixture', cargo_profile='synthetic', source_revision=None, dirty=False)
worker = dict(executable=store('inputs/worker/executable', b'synthetic worker bytes; never execute\n'),
              source=store('inputs/worker/source.bundle', b'synthetic source\n'),
              cargo_lock=store('inputs/worker/Cargo.lock', b'# synthetic lock\n'), build=build,
              platform=dict(os='linux', architecture='x86_64', abi='synthetic-fixture', requirements=['hand-authored contract evidence only']))
capabilities = dict(private_protocol=1, manifest_schema=1, event_schema=1, controller_simulation=True, real_model=False, real_workshop=False, recovery_profile='controller_archive_v1')
record = dict(schema_version=1, source_sha256=worker['source']['sha256'], lock_sha256=worker['cargo_lock']['sha256'], provenance=build,
              cargo_version='synthetic', linker_version='synthetic', features=[], rustflags=[], capabilities=capabilities)
build_ref = store('inputs/worker/build.json', record)
bundle_ref = store('inputs/worker/bundle.json', dict(schema_version=1, worker=worker, build_record=build_ref, capabilities=capabilities))
contract = dict(schema_version=1, rule_set='controller_archive_v1', event_schema_version=1, bridge_schema_version=1, observation_schema_version=1,
                required_event_kinds=sorted(['run_started','phase_changed','session_started','request_finished','command_finished','session_ended','checkpoint_published','run_finished','effect_intent','effect_not_dispatched','request_not_dispatched','stop_decided','observation_omitted','controller_observation']),
                effect_kinds=['cleanup','model','publish_checkpoint','snapshot','start_session','tool'], worker_executable=worker['executable'], configuration=configuration,
                generated_tokens=1000, log_bytes=1048576, storage_limits=limits, finalization_reserve_bytes=65536,
                observation_accounting='jsonl_plus_each_retained_payload_v1')
contract_ref = store('inputs/execution-contract.json', contract)
manifest = dict(schema_version=1, run_id=RUN, worker=worker, configuration=configuration, initial_workspace=workspace, files=[bundle_ref,build_ref,contract_ref])
manifest_ref = store('inputs/manifest.json', manifest)
descriptor = dict(schema_version=1, run_id=RUN, mode='simulation', input_manifest=manifest_ref)
archive = dict(run_id=RUN, descriptor=store('archive.json', descriptor), input_manifest=manifest_ref)

def checkpoint(n):
    return dict(session_id=f's{n}', workspace=store(f'sessions/s{n}/workspace.json', raw({'synthetic_checkpoint':n})+b'\n'),
                memento=dict(status='available',content=store(f'sessions/s{n}/memento.txt',f'memory {n}\n'.encode())), evidence=None)
cp1,cp2 = checkpoint(1),checkpoint(2)

def event(seq,kind,data):
    # serde_json Value payload objects have sorted keys; envelopes are structs.
    data = json.loads(json.dumps(data,sort_keys=True))
    return raw(dict(schema_version=1,run_id=RUN,sequence=seq,time=TIME,kind=kind,data=data))+b'\n'

def intent(effect,request):
    return dict(schema_version=1,effect_id=effect,effect_kind='model',session_id='s1',request_id=request,tool_request_sequence=None,maximum_tokens=100,deadline_ms=1000,sources=[])
lines = [event(1,'run_started',dict(mode='simulation',input_manifest=manifest_ref)),
         event(2,'effect_intent',intent(1,'r1')),
         event(3,'request_finished',dict(session_id='s1',request_id='r1',usage=dict(status='known',generated_tokens=10))),
         event(4,'checkpoint_published',dict(checkpoint=cp1)),
         event(5,'effect_intent',intent(2,'r2')),
         event(6,'stop_decided',dict(schema_version=1,reason='agent_finished',elapsed_ms=100)),
         event(7,'checkpoint_published',dict(checkpoint=cp2))]
base_bytes = b''.join(lines)

def frontier(seq,prefix):
    marker = dict(schema_version=1,run_id=RUN,committed_sequence=seq,committed_bytes=len(prefix),prefix_sha256=hashlib.sha256(prefix).hexdigest())
    return dict(archive=archive,marker=ref('events.commit.json',raw(marker)),committed_sequence=seq,committed_bytes=len(prefix),prefix_sha256=marker['prefix_sha256']),raw(marker)
base,marker_bytes = frontier(7,base_bytes)
store('events.commit.json',marker_bytes)
store('events.jsonl',base_bytes)
saved,_ = frontier(4,b''.join(lines[:4]))
confirmed = dict(schema_version=1,worker=saved,public=dict(frontier=saved,provenance=dict(kind='worker')))
binding = dict(schema_version=1,archive=archive,worker=dict(service_id='11111111111111111111111111111111',run_id=RUN,manifest_sha256=manifest_ref['sha256'],executable_sha256=worker['executable']['sha256'],grant_id='22222222222222222222222222222222',worker_id='33333333333333333333333333333333',invocation_id='44444444444444444444444444444444',boot_id='55555555555555555555555555555555',unit='playtime-44444444444444444444444444444444.service',kind='controller_simulation'),original_generation=1,superseding_generation=2)
attempt = dict(schema_version=1,attempt_id=ATTEMPT,chosen_time=TIME,binding=binding,provenance=dict(kind='owned_fixture'),maintenance_limits=maintenance)
inventory = dict(schema_version=1,attempt_id=ATTEMPT,archive=archive,base=base,entries=[])
inventory_ref = store(f'recovery/{ATTEMPT}/diagnostics.json',inventory)
source = lambda seq: dict(archive_sequence=seq,record_sha256=hashlib.sha256(lines[seq-1]).hexdigest())
usage = lambda known,reserved,unknown: dict(known_generated_tokens=known,unknown_reserved_tokens=reserved,unknown_requests=unknown,budget_charged_tokens=known+reserved,usage_complete=unknown==0)
plan = dict(schema_version=1,attempt=attempt,archive=archive,base=base,prior_bounds=confirmed,execution_contract=contract_ref,initial_inventory=inventory_ref,
            unresolved=[dict(source=source(5),effect_id=2,session_id='s1',request_id='r2',maximum_tokens=100)],
            first_stop=dict(source=source(6),reason='agent_finished'),last_checkpoint=dict(source=source(7),checkpoint=cp2),
            existing_usage=usage(10,0,0),resulting_usage=usage(10,100,1),base_accounted_log_bytes=len(base_bytes),available_log_bytes=1048576-len(base_bytes))
plan_ref = store(f'artifacts/recovery/{ATTEMPT}/plan.json',plan)
origin = dict(schema_version=1,attempt_id=ATTEMPT,plan=plan_ref)
patch = event(8,'request_finished',dict(session_id='s1',request_id='r2',usage=dict(status='unknown',reserved_tokens=100),recovery_origin=origin))
patch += event(9,'run_finished',dict(reason='agent_finished',usage=usage(10,100,1),cleanup=dict(status='unknown'),last_checkpoint=cp2,recovery_origin=origin))
patch_ref = store(f'recovery/{ATTEMPT}/patch.jsonl',patch)
target,target_bytes = frontier(9,base_bytes+patch)
target_ref = store(f'recovery/{ATTEMPT}/marker.json',target_bytes)
ordinary = {r['path']:r for r in [manifest_ref,configuration,workspace,*[worker[k] for k in ['executable','source','cargo_lock']],bundle_ref,build_ref,contract_ref,cp1['workspace'],cp1['memento']['content'],cp2['workspace'],cp2['memento']['content'],plan_ref]}
selection = dict(schema_version=1,attempt_id=ATTEMPT,archive=archive,base=base,plan=plan_ref,inventory=inventory_ref,patch=patch_ref,target_marker=target_ref,final_frontier=target,
                 capacity=dict(base_accounted_log_bytes=len(base_bytes),patch_bytes=len(patch),final_accounted_log_bytes=len(base_bytes)+len(patch),frozen_log_bytes=1048576,remaining_log_bytes=1048576-len(base_bytes)-len(patch),event_count=9,reference_count=len(ordinary),referenced_content_bytes=sum(r['size_bytes'] for r in ordinary.values())))
store('recovery/intent.json',selection)
for name,value in [('binding',binding),('attempt',attempt),('bounds',confirmed),('source',source(5)),('origin',origin)]:
    store(name+'.json',value)
for kind,body in [('terminal',dict(kind='terminal_preserved',frontier=target)),('never-started',dict(kind='never_started',evidence_sha256=hashlib.sha256(base_bytes).hexdigest())),('abandoned',dict(kind='abandoned_incomplete',evidence_sha256=hashlib.sha256(base_bytes).hexdigest(),operation_id='66666666666666666666666666666666',review_id='77777777777777777777777777777777',reason='Synthetic operator review only',accounting=dict(status='unavailable')))]:
    store('resolution-'+kind+'.json',dict(schema_version=1,archive=archive,attempt_id=ATTEMPT,resolution=body))
for path,data in files.items():
    destination = ROOT/path
    destination.parent.mkdir(parents=True,exist_ok=True)
    destination.write_bytes(data)
(ROOT/'hashes.json').write_text(json.dumps({p:ref(p,b) for p,b in sorted(files.items())},indent=2)+'\n')
print('base',len(base_bytes),'patch',len(patch),'plan',plan_ref['size_bytes'],'references',len(ordinary))
