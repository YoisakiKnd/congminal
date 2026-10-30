#!/usr/bin/env python3
"""Build/test-time only: fetch a fixed official Microsoft ConPTY pair and verify every byte."""
import argparse,hashlib,base64,io,json,os,shutil,subprocess,tempfile,urllib.request,zipfile
from pathlib import Path
root=Path(__file__).resolve().parent.parent
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--destination',type=Path,action='append',required=True);p.add_argument('--archive',type=Path);p.add_argument('--verify-signature',action='store_true');args=p.parse_args()
manifest=json.loads((root/'ci/conpty-runtime.json').read_text())
if args.archive: data=args.archive.read_bytes()
else:
 with urllib.request.urlopen(manifest['url'],timeout=60) as response:data=response.read(4*1024*1024+1)
assert len(data)<=4*1024*1024 and hashlib.sha256(data).hexdigest()==manifest['sha256'],'ConPTY package hash/size mismatch'
if 'nuget_sha512' in manifest:assert base64.b64encode(hashlib.sha512(data).digest()).decode()==manifest['nuget_sha512'],'NuGet SHA512 mismatch'
if args.verify_signature:
 with tempfile.TemporaryDirectory(prefix='congminal-nuget-') as tmp:
  pkg=Path(tmp)/'conpty.nupkg';pkg.write_bytes(data)
  subprocess.run(['dotnet','nuget','verify',str(pkg),'--all'],check=True,timeout=90,env=dict(os.environ,DOTNET_CLI_TELEMETRY_OPTOUT='1',DOTNET_NOLOGO='1',DOTNET_SKIP_FIRST_TIME_EXPERIENCE='1'))
with zipfile.ZipFile(io.BytesIO(data)) as archive:
 contents={}
 for name,item in manifest['files'].items():
  payload=archive.read(item['member']);assert len(payload)==item['bytes'] and hashlib.sha256(payload).hexdigest()==item['sha256'],name
  assert payload[:2]==b'MZ',name
  contents[name]=payload
 for dest in args.destination:
  dest.mkdir(parents=True,exist_ok=True)
  for name,payload in contents.items():
   output=dest/name;output.parent.mkdir(parents=True,exist_ok=True)
   if output.exists():assert output.read_bytes()==payload,'Refuse to replace a different runtime: '+str(output)
   else:output.write_bytes(payload)
  for name in ['LICENSE-ConPTY.txt','NOTICE-ConPTY.md']:
   license_file=root/'assets/licenses'/name
   if license_file.exists():shutil.copyfile(license_file,dest/name)
print(json.dumps({'package':manifest['package'],'version':manifest['version'],'verified_bytes':sum(len(v) for v in contents.values()),'destinations':[str(p) for p in args.destination]}))
