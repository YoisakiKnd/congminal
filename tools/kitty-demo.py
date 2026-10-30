#!/usr/bin/env python3
"""Display a local PNG through Kitty direct transfer and Unicode placeholders. No image libraries."""
import argparse,base64,os,re,sys,time
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__);p.add_argument('image',type=Path,nargs='?',default=Path(__file__).resolve().parents[1]/'assets/icons/congminal-256.png');p.add_argument('--hold',type=float,default=0);args=p.parse_args()
if os.name=='nt':
 import ctypes
 k=ctypes.WinDLL('kernel32',use_last_error=True);k.GetStdHandle.restype=ctypes.c_void_p;k.GetConsoleMode.argtypes=[ctypes.c_void_p,ctypes.POINTER(ctypes.c_uint)];k.SetConsoleMode.argtypes=[ctypes.c_void_p,ctypes.c_uint]
 handle=k.GetStdHandle(-11);mode=ctypes.c_uint()
 if k.GetConsoleMode(handle,ctypes.byref(mode)):k.SetConsoleMode(handle,mode.value|4)
raw=args.image.read_bytes();assert raw[:8]==b'\x89PNG\r\n\x1a\n' and len(raw)<=16*1024*1024
codes=re.findall(r"'\\u\{([0-9a-fA-F]+)\}'",(Path(__file__).resolve().parents[1]/'src/graphics/placeholder.rs').read_text())
# Protocol-data table appears after the standalone U+10EEEE declaration.
codes=[chr(int(v,16)) for v in codes if int(v,16)!=0x10eeee];assert len(codes)==297
wire=bytearray(b'\x1b[2J\x1b[HKitty graphics / congminal\r\n');encoded=base64.b64encode(raw)
for at in range(0,len(encoded),4096):
 more=int(at+4096<len(encoded));head=f'i=42,a=T,U=1,f=100,c=24,r=12,q=2,m={more}' if at==0 else f'q=2,m={more}'
 wire+=b'\x1b_G'+head.encode()+b';'+encoded[at:at+4096]+b'\x1b\\'
for y in range(12):
 wire+=f'\x1b[{y+3};3H\x1b[38;2;0;0;42m'.encode()
 wire+=('\U0010eeee'+codes[y]+codes[0]+codes[0]+'\U0010eeee'*23).encode()
wire+=b'\x1b[39m\x1b[16;1HKitty PNG + Unicode placeholders\r\n';sys.stdout.buffer.write(wire);sys.stdout.buffer.flush()
if args.hold>0:time.sleep(min(args.hold,600))
