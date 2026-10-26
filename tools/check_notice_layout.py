#!/usr/bin/env python3
"""临时发布副本早返回布局探针：不打开窗口，不启用通知工具。"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
from check_font_discovery import sources
from check_sftp_executor import run


def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--output',type=Path,required=True);args=parser.parse_args();root=Path(__file__).resolve().parent.parent
    injection='''fn main() {
    if std::env::var_os("CONGMINAL_NOTICE_LAYOUT").is_some() {
        println!("NOTICE_LAYOUT item={} center={} budget={}",std::mem::size_of::<notifications::Notice>(),std::mem::size_of::<notifications::Center>(),std::mem::size_of::<resources::Budget>());
        return;
    }
'''
    result=dict(scope='Temporary native release project, same source with env-gated early main return printing type layout only. Runs default and sftp-preview binary without opening a window, terminal/helper or notification tool. Not heap/RSS/input/system delivery acceptance.',source_sha256=sources(root),manifests_sha256={n:hashlib.sha256((root/n).read_bytes()).hexdigest() for n in ['Cargo.toml','Cargo.lock']},tool_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),injection=injection,checks=[])
    try:
        with tempfile.TemporaryDirectory(prefix='cg-notice-layout-') as temporary:
            p=Path(temporary)
            for n in ['src','assets']:shutil.copytree(root/n,p/n)
            for n in ['Cargo.toml','Cargo.lock']:shutil.copy2(root/n,p/n)
            main=p/'src/main.rs';text=main.read_text();assert text.count('fn main() {')==1;main.write_text(text.replace('fn main() {',injection,1))
            result['rustc']=run(['rustc','-Vv'],p,15)
            for mode,features in [('default',[]),('preview',['--features','sftp-preview'])]:
                built=run(['cargo','build','--offline','--release','--bin','congminal']+features,p,240)
                if built['exit']:raise RuntimeError(str(built))
                import os
                proc=subprocess.run([str(p/'target/release/congminal')],cwd=p,env=dict(os.environ,CONGMINAL_NOTICE_LAYOUT='1'),capture_output=True,text=True,timeout=10)
                match=re.search(r'NOTICE_LAYOUT item=(\d+) center=(\d+) budget=(\d+)',proc.stdout)
                if proc.returncode or match is None:raise RuntimeError(proc.stdout+proc.stderr)
                result['checks'].append(dict(mode=mode,build=built,exit=proc.returncode,stdout=proc.stdout,stderr=proc.stderr,item=int(match[1]),center=int(match[2]),budget=int(match[3])))
        assert sources(root)==result['source_sha256'];result['passed']=True
    finally:args.output.write_text(json.dumps(result,indent=2)+'\n')


if __name__=='__main__':main()
