#!/usr/bin/env python3
"""Bind the complete MCP source migration without claiming native compilation."""
from pathlib import Path
import ast, collections, hashlib, json, re, subprocess, tomllib, argparse
import yaml
P=argparse.ArgumentParser();P.add_argument('--output',required=True);A=P.parse_args()
W=Path(__file__).resolve().parent.parent;S=W/'crates/dione-mcp-session/src';BASE='a6b2c5586d57fd5006c340888e340af80c4c86f5'
ns={'re':re,'collections':collections}
for node in ast.parse((W/'scripts/verify-recipient-attention-migration.py').read_text()).body:
 if isinstance(node,ast.FunctionDef) and node.name in ['tokens','funcs']:
  exec(compile(ast.Module(body=[node],type_ignores=[]),'maintained lexical reader','exec'),ns)
def old(path):return subprocess.check_output(['git','show',BASE+':'+path],cwd=W,text=True)
def norm(t):
 t=t.replace('dione::mcp::','crate::mcp::')
 t=t.replace('dione_mcp_session::','crate::mcp::server::')
 t=t.replace('server.attention_runtime().clone()', 'server.attention.clone()')
 t=re.sub(r'DioneServer::new\(\s*env!\("CARGO_PKG_VERSION"\)\s*,','DioneServer::new(',t)
 t=re.sub(r'DioneServer::new\(\s*crate::TEST_APPLICATION_VERSION\s*,','DioneServer::new(',t)
 t=t.replace('crate::TEST_APPLICATION_VERSION','env!("CARGO_PKG_VERSION")')
 t=t.replace('server.application_version','env!("CARGO_PKG_VERSION")')
 t=t.replace('get_version(env!("CARGO_PKG_VERSION"))','get_version()')
 t=t.replace('"version": application_version','"version": env!("CARGO_PKG_VERSION")')
 t=t.replace('            application_version,\n','')
 t=t.replace('_notification_tx: notification_tx','notification_tx')
 t=re.sub(r'insta::assert_json_snapshot!\(\s*"mcp_protocol__[^"\n]+",\s*','insta::assert_json_snapshot!(',t)
 t=re.sub(r'    let mut snapshot_settings = insta::Settings::clone_current\(\);\s*snapshot_settings.set_prepend_module_to_snapshot\(false\);\s*let _snapshot_scope = snapshot_settings.bind_to_scope\(\);','',t)
 t=t.replace('    assert_ne!(parsed["version"], env!("CARGO_PKG_VERSION"));','')
 return t
def key(t):
 ts=[x[0] for x in ns['tokens'](norm(t))]
 return tuple(v for i,v in enumerate(ts) if not(v==',' and i+1<len(ts) and ts[i+1] in [')',']','}']))
paths={}
for p in (S/'mcp').rglob('*.rs'):paths['src/'+str(p.relative_to(S))]=p
for name in ['harness_test_support.rs','introspection_test_support.rs','teams_test_process.rs','codex_test_process.rs','codex_test_wire.rs','codex_dispatch_tests.rs']:
 paths['src/'+name]=S/name
for name in ['management.rs','reply.rs']:paths['src/discord_operations_tests/'+name]=S/'discord_operations_tests'/name
paths['tests/mcp_protocol.rs']=S/'protocol_integration.rs';paths['tests/config_store.rs']=S/'config_store_tests.rs';paths['tests/support/codex_projection.rs']=S/'support/codex_projection.rs'
paths['src/startup.rs']=W/'src/startup.rs'
checks=[];fail=[]
for source,dest in paths.items():
 after=collections.Counter((x['name'],key(x['body'])) for x in ns['funcs'](dest.read_text()))
 for f in ns['funcs'](old(source)):
  if source=='src/mcp/server.rs' and f['name']=='ingress_ledger':
   assert not any('.ingress_ledger()' in p.read_text() for p in S.rglob('*.rs'))
   checks.append({'source':source,'function':f['name'],'disposition':'unused former public getter removed after private migration; no new owner caller'})
   continue
  pair=f['name'],key(f['body']);ok=after[pair]>0
  if ok:after[pair]-=1
  checks.append({'source':source,'destination':str(dest.relative_to(W)),'function':f['name'],'body_equal_after_declared_adapters':ok})
  if not ok:fail.append([source,f['name']])
# Snapshot payload bytes and explicit names remain the predecessor evidence.
snaps=[]
for p in (S/'snapshots').glob('*.snap'):
 assert p.read_bytes()==subprocess.check_output(['git','show',BASE+':tests/snapshots/'+p.name],cwd=W)
 snaps.append(p.name)
root=tomllib.loads((W/'Cargo.toml').read_text());workspace=root['workspace']['dependencies'];docs={};edge={};dev={}
for p in [W/'Cargo.toml',*sorted((W/'crates').glob('*/Cargo.toml'))]:
 d=tomllib.loads(p.read_text());docs[d['package']['name']]=d
for n,d in docs.items():
 edge[n]=set();dev[n]=set();tables=[d]+list(d.get('target',{}).values())
 for tab in tables:
  for section,dest in [('dependencies',edge[n]),('dev-dependencies',dev[n]),('build-dependencies',edge[n])]:
   for alias,v in tab.get(section,{}).items():
    if not isinstance(v,dict):continue
    v=workspace[alias] if v.get('workspace') else v
    if 'path' in v:dest.add(v.get('package',alias))
visiting=set();seen=set()
def visit(n):
 assert n not in visiting,('cycle',n)
 if n in seen:return
 visiting.add(n)
 for dep in edge[n]:visit(dep)
 visiting.remove(n);seen.add(n)
for n in docs:visit(n)
jobs=yaml.safe_load((W/'.github/workflows/publish-crate.yml').read_text())['jobs'];jseen=set();active=set()
def jvisit(n):
 assert n not in active,('release cycle',n)
 if n in jseen:return
 active.add(n);deps=jobs[n].get('needs',[]);deps=[deps] if isinstance(deps,str) else deps
 for dep in deps:jvisit(dep)
 active.remove(n);jseen.add(n)
for n in jobs:jvisit(n)
prereqs=edge['dione-mcp-session']|dev['dione-mcp-session']
for name in ['publish-mcp-session','reconcile-mcp-session']:
 for dep in prereqs:
  short=dep.removeprefix('dione-');assert 'reconcile-'+short in jobs[name]['needs'];assert "needs.reconcile-"+short+".result == 'success'" in jobs[name]['if']
lock=tomllib.loads((W/'Cargo.lock').read_text());before=tomllib.loads(old('Cargo.lock'));ext=lambda d:{(x['name'],x['version'],x['source']):x for x in d['package'] if x.get('source')};assert ext(lock)==ext(before)
result={'baseline':BASE,'function_checks':checks,'mismatches':fail,'snapshots_exact':snaps,'normal_packages':len(docs),'normal_graph_acyclic':True,'normal_owner_dependencies':sorted(edge['dione-mcp-session']),'packaged_dev_prerequisites':sorted(prereqs),'release_jobs':len(jobs),'release_graph_acyclic':True,'external_records_and_edges_unchanged':len(ext(lock)),'limits':'Source lexical binding only. Actual test list, lifetimes, features and effects remain uncompiled.'}
Path(A.output).write_text(json.dumps(result,indent=2));print(json.dumps({k:v for k,v in result.items() if k!='function_checks'},indent=2));assert not fail,fail
