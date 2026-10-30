from pathlib import Path
import hashlib,json,struct,sys,tarfile,zipfile,plistlib
root=Path(sys.argv[1]); expected=sys.argv[2]; records=[]
archives=sorted(list(root.glob('*.zip'))+list(root.glob('*.tar.gz')))
assert len(archives)==3, archives
for archive in archives:
    checksum=Path(str(archive)+'.sha256').read_text().split()
    digest=hashlib.sha256(archive.read_bytes()).hexdigest()
    assert checksum[0]==digest and checksum[1].lstrip('*')==archive.name
    if archive.suffix=='.zip':
        z=zipfile.ZipFile(archive)
        # ditto writes UTF-8 bytes without ZIP bit 11; preserve original lookup keys.
        def zip_name(info):
            if info.flag_bits & 0x800: return info.filename
            try: return info.filename.encode('cp437').decode('utf-8')
            except (UnicodeEncodeError,UnicodeDecodeError): return info.filename
        entries={zip_name(info):info for info in z.infolist()}
        names=list(entries); read=lambda n:z.read(entries[n])
    else:
        z=tarfile.open(archive); names=z.getnames(); read=lambda n:z.extractfile(n).read()
    def one(suffix):
        found=[n for n in names if n.endswith('/'+suffix) and not n.startswith('__MACOSX/')]
        assert len(found)==1,(suffix,found)
        return found[0]
    build=read(one('BUILD.txt')).decode()
    assert expected in build and '0.1.0' in build and '未提交' not in build
    for filename in ['README.md','congminal.example.toml','LICENSE-Terminus.txt','congminal.zsh','congminal.bash']:
        assert len(read(one(filename)))>0
    if 'macos' in archive.name:
        pl=plistlib.loads(read(one('congminal.app/Contents/Info.plist')))
        assert pl['CFBundleShortVersionString']=='0.1.0'
        paths=[one('congminal.app/Contents/MacOS/'+n) for n in ['congminal-bin','congminal-ssh-helper']]
        for n in paths:
            data=read(n); assert data[:4]==bytes.fromhex('cafebabe')
            count=struct.unpack_from('>I',data,4)[0]
            cpu={struct.unpack_from('>I',data,8+i*20)[0] for i in range(count)}
            assert cpu=={0x1000007,0x100000c}, cpu
        for n in paths: assert (entries[n].external_attr >> 16) & 0o111
        assert len(read(one('使用说明.md')))>0
        architecture='universal arm64+x86_64 (main and helper)'
    elif 'windows' in archive.name:
        paths=[one(n) for n in ['congminal.exe','congminal-ssh-helper.exe']]
        for n in paths:
            data=read(n); assert data[:2]==b'MZ'; offset=struct.unpack_from('<I',data,0x3c)[0]
            assert data[offset:offset+4]==b'PE\0\0' and struct.unpack_from('<H',data,offset+4)[0]==0x8664
        assert len(read(one('RELEASE-NOTES.md')))>0
        architecture='PE x86_64 (main and helper)'
    else:
        paths=[one(n) for n in ['congminal','congminal-ssh-helper']]
        for n in paths:
            data=read(n); assert data[:5]==b'\x7fELF\x02' and struct.unpack_from('<H',data,18)[0]==62
        for n in paths: assert z.getmember(n).mode & 0o111
        assert len(read(one('RELEASE-NOTES.md')))>0
        architecture='ELF x86_64 (main and helper)'
    records.append(dict(asset=archive.name,bytes=archive.stat().st_size,sha256=digest,build=build,architecture=architecture,required_files=True))
print(json.dumps(dict(expected_commit=expected,assets=records,all_checks_passed=True),ensure_ascii=False,indent=2))
