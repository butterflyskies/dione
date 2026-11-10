#!/usr/bin/env python3
"""Verify recipient migration bodies and dependency/DAG facts without compiling Rust.

This lexical oracle does not establish lifetimes, effects or registry readiness.
The private rpc fixture is explicitly owner-control evidence after the move.
"""
from pathlib import Path
import argparse, collections, hashlib, json, re, subprocess, tomllib
import yaml
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--baseline', default='31fcd15ed5dfac6de2df031e7a4420704f787728')
parser.add_argument('--output', required=True)
args=parser.parse_args()
W=Path(__file__).resolve().parent.parent
S=W/'crates/dione-recipient-attention/src'
def old(path): return subprocess.check_output(['git','show',args.baseline+':'+path],cwd=W,text=True)
def normalize(s):
 return s.replace('crate::attention::runtime::','crate::runtime::').replace('crate::attention::control::','crate::control::').replace('crate::discord_bells_tests::tests::McpFixture','crate::bell_test_support::McpFixture')
def tokens(s):
 out=[];i=0
 while i<len(s):
  if s[i].isspace(): i+=1;continue
  if s.startswith('//',i):
   j=s.find('\n',i);i=len(s) if j<0 else j;continue
  if s.startswith('/*',i):
   n=1;i+=2
   while n and i<len(s):
    if s.startswith('/*',i):n+=1;i+=2
    elif s.startswith('*/',i):n-=1;i+=2
    else:i+=1
   assert n==0;continue
  start=i
  raw=re.match(r'(?:br|cr|r)(#{0,255})"',s[i:])
  if raw:
   end='"'+raw[1];j=s.find(end,i+raw.end());assert j>=0;i=j+len(end)
  elif s[i]=='"' or (s[i] in 'bc' and s[i:i+2] in ['b"','c"']):
   if s[i]!='"':i+=1
   i+=1
   while i<len(s):
    if s[i]=='\\':i+=2
    elif s[i]=='"':i+=1;break
    else:i+=1
  elif s[i]=="'" and re.match(r"'(?:\\(?:u\{[^}]+\}|x..|.)|[^'\n\\])'",s[i:]):
   i+=re.match(r"'(?:\\(?:u\{[^}]+\}|x..|.)|[^'\n\\])'",s[i:]).end()
  else:
   m=re.match(r'[A-Za-z_][A-Za-z_0-9]*|[0-9][A-Za-z_0-9]*',s[i:]);i+=m.end() if m else 1
  out.append((s[start:i],start,i))
 return out

def funcs(s):
 ts=tokens(s);stack=[];pairs={};out=[]
 for i,(t,a,b) in enumerate(ts):
  if t=='{':stack.append(i)
  elif t=='}':pairs[stack.pop()]=i
 assert not stack
 for i,(t,a,b) in enumerate(ts):
  if t!='fn' or i+1>=len(ts) or not re.fullmatch(r'\w+',ts[i+1][0]):continue
  j=i+2
  while j<len(ts) and ts[j][0] not in [';','{']:j+=1
  if j==len(ts) or ts[j][0]==';':continue
  e=pairs[j];out.append({'name':ts[i+1][0],'line':s.count('\n',0,a)+1,'body':s[ts[j][1]:ts[e][2]]})
 return out
def key(body):
 ts=[x[0] for x in tokens(normalize(body))]
 # Ignore only optional formatting trailing commas, never literal contents.
 return tuple(v for i,v in enumerate(ts) if not (v==',' and i+1<len(ts) and ts[i+1] in [')',']','}']))
checks=[]
def compare(path,dest,names=None,excluded=()):
 after=collections.Counter((x['name'],key(x['body'])) for x in funcs(dest.read_text()))
 for f in funcs(old(path)):
  if names is not None and f['name'] not in names:continue
  if f['name'] in excluded:continue
  pair=(f['name'],key(f['body']));same=after[pair]>0
  if same:after[pair]-=1
  checks.append({'source':path,'destination':str(dest.relative_to(W)),'name':f['name'],'before_line':f['line'],'same_body':same})
compare('src/attention/runtime.rs',S/'runtime.rs')
compare('src/attention/control.rs',S/'control.rs')
compare('src/attention/store_access.rs',S/'store_access.rs')
for name in ['attention_tests.rs','attention_boundary_acceptance.rs','attention_lifecycle_acceptance.rs','scheduling_tests.rs']:
 compare('src/mcp/'+name,S/name,excluded=['rpc'])
forward=['ok','into_result','presentation','deliver_flushed','message_rate_limit_key','extract_delay','forward_ordinary','forward_attention_results','attention_transition','begin_attention','test_connection_for_queue']
server=old('src/mcp/server.rs');tests=server[server.index('    fn config_with_channel_delay'):server.index('/// The same forwarding unit')]
forward+= [x['name'] for x in funcs(tests[:tests.rfind('}')])]
compare('src/mcp/server.rs',S/'forwarding.rs',names=forward)
retained=['resident_control_allows_only_bounded_tools','resident_social_tools_are_the_exact_closed_bridge_surface','resident_control_frames_are_bounded_and_newline_delimited','resident_shutdown_waits_for_accepted_reply_after_client_disconnect','messaging_context_uses_process_installed_pipeline']
compare('src/mcp/server.rs',W/'src/mcp/server.rs',names=retained)
# Exact moved loop, including all abort/join ordering and final flush. Only assembly names change.
a=server.index('        let mut rx = event_rx;');b=server.index('\n    });\n\n    // Main request loop.',a)
loop=key(server[a:b]);target=key((S/'forwarding.rs').read_text());loop_preserved=any(target[i:i+len(loop)]==loop for i in range(len(target)-len(loop)+1))
# No external resolver changes. Local manifests supply both graph and package prerequisites.
root=tomllib.loads((W/'Cargo.toml').read_text());members={}
for path in root['workspace']['members']:
 p=W/path/'Cargo.toml';m=tomllib.loads(p.read_text());members[m['package']['name']]=(path,m)
graph={n:sorted(x for x in m.get('dependencies',{}) if x in members) for n,(_,m) in members.items()}
visited=set();active=set()
def visit(n):
 assert n not in active, 'normal dependency cycle: '+n
 if n in visited:return
 active.add(n)
 for child in graph[n]:visit(child)
 active.remove(n);visited.add(n)
for n in graph:visit(n)
new=members['dione-recipient-attention'][1]
prereqs=sorted({n for cat in ['dependencies','dev-dependencies'] for n in new.get(cat,{}) if n in members})
workflow=yaml.safe_load((W/'.github/workflows/publish-crate.yml').read_text())['jobs']
seen=set();active=set()
def jobvisit(n):
 assert n not in active,'release DAG cycle: '+n
 if n in seen:return
 active.add(n)
 deps=workflow[n].get('needs',[])
 if isinstance(deps,str):deps=[deps]
 for child in deps:jobvisit(child)
 active.remove(n);seen.add(n)
for n in workflow:jobvisit(n)
for job in ['publish-recipient-attention','reconcile-recipient-attention']:
 for name in prereqs:
  dep='reconcile-'+name.removeprefix('dione-')
  assert dep in workflow[job]['needs'],(job,dep)
  assert "needs."+dep+".result == 'success'" in workflow[job]['if'],(job,dep,'guard')
for job in ['publish-crate','reconcile-dione']:
 assert 'reconcile-recipient-attention' in workflow[job]['needs']
 assert "needs.reconcile-recipient-attention.result == 'success'" in workflow[job]['if']
before=tomllib.loads(old('Cargo.lock'));after=tomllib.loads((W/'Cargo.lock').read_text())
ext=lambda d:sorted([x for x in d['package'] if 'source' in x],key=lambda x:(x['name'],x['version']))
external_equal=ext(before)==ext(after)
normal_reverse=['dione-recipient-attention'];changed=True
while changed:
 changed=False
 for n,deps in graph.items():
  if n not in normal_reverse and set(deps)&set(normal_reverse):normal_reverse.append(n);changed=True
names=[]
for p in S.glob('*.rs'):
 for m in re.finditer(r'#\[(?:tokio::)?test(?:\([^\]]*\))?\]\s*(?:async )?fn (\w+)',p.read_text()):names.append({'file':p.name,'name':m[1]})
result={'baseline':args.baseline,'body_checks':checks,'body_count':len(checks),'body_mismatches':[x for x in checks if not x['same_body']], 'whole_notification_loop_preserved':loop_preserved,'private_rpc_helper_delta':'DioneServer construction/private injection/root dispatch replaced with owner execute and private response envelope; moved cases no longer root wire evidence. Separate ordinary root caller case added.', 'private_fixture_adaptations':'Codex process helper keeps used methods only; private Bell loopback specializes existing successful Value response; gateway handler and wire helpers copied privately. No public fixture feature.', 'member_count':len(graph),'normal_graph':graph,'owner_normal_dependencies':graph['dione-recipient-attention'],'owner_packaged_local_prerequisites':prereqs,'normal_reverse_release_closure':sorted(normal_reverse),'workflow_jobs':len(workflow),'external_count':len(ext(after)),'external_records_equal':external_equal,'owner_test_attributes':names,'limits':'Source lexical/manifest proof only; Cargo resolution, visibility, features and all runtime effects remain uncompiled.'}
Path(args.output).write_text(json.dumps(result,indent=2)+'\n')
print('bodies',len(checks),'mismatches',len(result['body_mismatches']),'loop',loop_preserved,'tests',len(names),'normal',len(graph['dione-recipient-attention']),'packaged',len(prereqs),'external',len(ext(after)),external_equal)
for x in result['body_mismatches']:print(x)
assert not result['body_mismatches'] and loop_preserved and external_equal
