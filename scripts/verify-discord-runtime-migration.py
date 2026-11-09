#!/usr/bin/env python3
"""Compare the gateway migration against immutable source bodies and assertions.

This is a lexical migration oracle, not Rust name resolution or runtime proof.
Only explicit namespace, concrete Http parameter and demonstrated formatting
adaptations are normalized. Assertion literal strings are never normalized.
"""
from pathlib import Path
import argparse, re, json, hashlib, collections, subprocess
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--baseline', required=True)
parser.add_argument('--output')
args=parser.parse_args()
W=Path(__file__).resolve().parent.parent
B=args.baseline
EXPECTED_SETUP_DELTAS = {'src/mcp/attention_lifecycle_acceptance.rs': {'removed': ['01a8bbacb3bd6c3cf5e22ac9df7ee5f096265690ff42a27182262faa3976626d', '0916b5ba8004664272cf9ccc699c1d06ed75da01675681ad5912f3da650c529e', '591ba66b24a42418b71ba09c1dafb736e06fcad509a0cddb5dba114fa4a0c96d', '591ba66b24a42418b71ba09c1dafb736e06fcad509a0cddb5dba114fa4a0c96d', '591ba66b24a42418b71ba09c1dafb736e06fcad509a0cddb5dba114fa4a0c96d'], 'added': ['01ca3887800d8cc93f9dee041cc95bf6eb432e04332783607537bbf86b2a8d04', '01ca3887800d8cc93f9dee041cc95bf6eb432e04332783607537bbf86b2a8d04', '01ca3887800d8cc93f9dee041cc95bf6eb432e04332783607537bbf86b2a8d04', 'adee4d7ea30ae1f63bf6499c6a6f402b51d80cc985c1fa550bc6bf2e8064eb90', 'f8956cae7b38d25eca82d8f1a8df3eb47ced290a56779eeb6bd777a07b914ff9']}}
# Same shared state Arc, now accessed through its root NoticeServices owner.
EXPECTED_SETUP_DELTAS['src/attention/runtime.rs'] = {'removed': ['08e33c4f72d24fa32954eab06d04e71e30f37dbd63c586f8eeca921028a2296c'], 'added': ['e2ad57348c746b0a7cbc96c9a6ad24e7849650250c2211607337f97bcf2f26c6']}
ROOT_CALLER_FILES = ['src/attention/control.rs', 'src/attention/runtime.rs', 'src/attention/source.rs', 'src/codex_test_process.rs', 'src/discord/client.rs', 'src/discord/events.rs', 'src/discord/management.rs', 'src/discord/management_tools.rs', 'src/discord/messaging/rook_publication.rs', 'src/discord/messaging.rs', 'src/discord/observation.rs', 'src/discord/query_context_tests.rs', 'src/discord/query_tools_tests.rs', 'src/discord/reply_tool.rs', 'src/discord/verified_action.rs', 'src/discord/verified_action_runtime.rs', 'src/discord/visibility_contract_tests.rs', 'src/ingress_ledger.rs', 'src/lib.rs', 'src/mcp/attention_boundary_acceptance.rs', 'src/mcp/attention_lifecycle_acceptance.rs', 'src/mcp/attention_tests.rs', 'src/mcp/dispatch.rs', 'src/mcp/harness_tests.rs', 'src/mcp/registration_tests.rs', 'src/mcp/render_wire.rs', 'src/mcp/server.rs', 'src/startup.rs', 'tests/config_store.rs', 'tests/contradictionary_flow.rs', 'tests/mcp_protocol.rs', 'tests/mute_composition.rs', 'tests/ui/verified_action/clone_action.rs', 'tests/ui/verified_action/construct_actions.rs', 'tests/ui/verified_action/construct_admission.rs', 'tests/ui/verified_action/gate_misuse.rs', 'tests/ui/verified_action/resolve_with_coordinates.rs', 'tests/ui/verified_action/reuse_action.rs', 'tests/ui/verified_action/reuse_admission_as_transport.rs', 'tests/ui/verified_action/swap_provider.rs']
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
paths={'src/discord/client.rs':'client.rs','src/discord/events.rs':'events.rs','src/discord/verified_action.rs':'verified_action.rs','src/discord/verified_action_runtime.rs':'verified_action_runtime.rs','src/ingress_ledger.rs':'ingress.rs','src/discord/observation.rs':'observation.rs','src/discord/rook_observation.rs':'rook_observation.rs','src/attention/source.rs':'source.rs','src/composition/mutes.rs':'registry.rs'}
def old(f):return subprocess.check_output(['git','show',B+':'+f],cwd=W,text=True)
def normalize(s):
 s=s.replace('GatewayHandler','Handler')
 for name in ['SourceFailure','SourceResolver','now_ms']:
  s=s.replace('dione_discord_runtime::'+name,'crate::attention::source::'+name)
 s=s.replace('crate::verified_action_runtime','crate::discord::verified_action_runtime').replace('crate::verified_action','crate::discord::verified_action').replace('crate::events','crate::discord::events').replace('crate::observation','crate::discord::observation')
 s=s.replace('crate::ingress::','crate::ingress_ledger::').replace('dione_discord_runtime::','crate::ingress_ledger::').replace('dione_mutes::','crate::composition::mutes::')
 s=s.replace('#[cfg(any(test, feature = "test-support"))]','#[cfg(test)]')
 s=s.replace('crate::introspection_test_support::fixture','super::super::introspection_test_support::fixture')
 return s

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

def tokenbody(s):return tuple(x[0] for x in tokens(normalize(s)))
owner=[];differences=[]
for p,to in paths.items():
 dest=W/('crates/dione-mutes/src/registry.rs' if to=='registry.rs' else 'crates/dione-discord-runtime/src/'+to)
 before=funcs(old(p));after=funcs(dest.read_text());matched=set()
 for f in before:
  name={'message':'process_message','message_update':'process_update','emit_lifecycle_delete':'process_delete'}.get(f['name'],f['name']) if to=='events.rs' else f['name']
  candidate=[(i,x) for i,x in enumerate(after) if x['name']==name and i not in matched]
  want=tokenbody(f['body']);chosen=None
  for i,g in candidate:
   body=g['body']
   if to=='events.rs' and name in ['process_message','process_update']:
    body=re.sub(r'\bhttp\b','&ctx.http',body)
    # rustfmt removed only trailing call commas after the shorter concrete parameter.
    actual=tokenbody(body);wanted=want
    actual=tuple(v for k,v in enumerate(actual) if not (v==',' and k+1<len(actual) and actual[k+1]==')'))
    wanted=tuple(v for k,v in enumerate(wanted) if not (v==',' and k+1<len(wanted) and wanted[k+1]==')'))
    if actual==wanted:chosen=i;break
   if to=='events.rs' and name in ['message_delete','message_delete_bulk']:body=body.replace('.process_delete(','.emit_lifecycle_delete(')
   if to=='source.rs' and name=='resolve_with_parent':body=body.replace('crate::ingress::SourceDeliveryFailure::Invalidated => SourceFailure::Invalidated,','crate::ingress::SourceDeliveryFailure::Invalidated => { SourceFailure::Invalidated }')
   if tokenbody(body)==want:chosen=i;break
  result={'file':p,'function':f['name'],'before_line':f['line'],'equal_normalized_body':chosen is not None}
  if chosen is not None:matched.add(chosen)
  else:
   result['candidates']=[x['line'] for i,x in candidate];differences.append(result)
  owner.append(result)
# Root assertion macro token multisets. A path spelling normalization is explicit.
def asserts(s):
 ts=tokens(normalize(s));out=[]
 for i,(t,a,b) in enumerate(ts):
  if t not in ['assert','assert_eq','assert_ne','debug_assert','debug_assert_eq','debug_assert_ne','prop_assert','prop_assert_eq','prop_assert_ne']:continue
  if i+2>=len(ts) or ts[i+1][0]!='!' or ts[i+2][0] not in ['(','{','[']:continue
  close={'(':')','{':'}','[':']'};stack=[close[ts[i+2][0]]];j=i+3
  while stack:
   z=ts[j][0]
   if z in close:stack.append(close[z])
   elif z==stack[-1]:stack.pop()
   j+=1
  out.append(tuple(x[0] for x in ts[i:j]))
 return collections.Counter(out)
root=[];inventory={"files": ROOT_CALLER_FILES}
for p in inventory['files']:
 if p in paths or p.startswith('tests/ui/'):continue
 dest=W/p
 if not dest.exists():continue
 a=asserts(old(p));b=asserts(dest.read_text());removed=list((a-b).elements());added=list((b-a).elements())
 root.append({'file':p,'before_assertions':sum(a.values()),'after_assertions':sum(b.values()),'removed':len(removed),'added':len(added),'removed_tokens':[' '.join(v) for v in removed],'added_tokens':[' '.join(v) for v in added]})
res={'baseline':B,'owner_functions':owner,'owner_body_differences':differences,'root_assertions':root,'normalization':'Concrete crate namespace/Handler rename, readonly cfg extension; SDK Http parameter expansion only in two moved event bodies. Root assertion comparison normalizes namespace only. No literal normalization.'}
if args.output: Path(args.output).write_text(json.dumps(res,indent=2)+'\n')
print('Owner bodies:',len(owner),'normalized exact:',sum(x['equal_normalized_body'] for x in owner),'differences:',len(differences))
for x in differences:print(x)
for x in root:
 if x['removed'] or x['added']:print('ASSERTION DELTA',x['file'],x['removed'],x['added'])

unexpected=[]
for record in root:
    actual={key:sorted(hashlib.sha256(value.encode()).hexdigest() for value in record[key+'_tokens']) for key in ['removed','added']}
    expected=EXPECTED_SETUP_DELTAS.get(record['file'], {'removed':[], 'added':[]})
    if actual != expected: unexpected.append(record['file'])
if unexpected: print('Unexpected assertion delta:', ', '.join(unexpected))
raise SystemExit(1 if differences or unexpected else 0)
