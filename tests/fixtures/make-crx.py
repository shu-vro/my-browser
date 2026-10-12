# Build a signed CRX3 from an extension folder, with a new RSA key that is
# thrown away afterwards. Prints the extension id.
#   mkdir /tmp/k && python3 -I tests/fixtures/make-crx.py /tmp/k tests/fixtures/title-extension tests/fixtures/title.crx > tests/fixtures/title.crx.id && rm -r /tmp/k
import subprocess, hashlib, struct, sys, os
d, fixture, out = sys.argv[1], sys.argv[2], sys.argv[3]
key = os.path.join(d, 'key.pem')
subprocess.run(['/usr/bin/openssl','genrsa','-out',key,'2048'], check=True, capture_output=True)
spki = subprocess.run(['/usr/bin/openssl','rsa','-in',key,'-pubout','-outform','DER'], check=True, capture_output=True).stdout
zp = os.path.join(d,'t.zip')
subprocess.run(['/usr/bin/ditto','-c','-k','--keepParent',fixture,zp], check=True)
z = open(zp,'rb').read()
def v(n):
    b=b''
    while True:
        x=n&0x7f; n>>=7
        if n: b+=bytes([x|0x80])
        else: return b+bytes([x])
def ld(f,data): return v((f<<3)|2)+v(len(data))+data
crx_id = hashlib.sha256(spki).digest()[:16]
signed = ld(1, crx_id)
msg = b'CRX3 SignedData\x00'+struct.pack('<I',len(signed))+signed+z
mp=os.path.join(d,'msg'); open(mp,'wb').write(msg)
sig = subprocess.run(['/usr/bin/openssl','dgst','-sha256','-sign',key,mp], check=True, capture_output=True).stdout
header = ld(2, ld(1,spki)+ld(2,sig)) + ld(10000, signed)
open(out,'wb').write(b'Cr24'+struct.pack('<II',3,len(header))+header+z)
print(''.join(chr(ord('a')+n) for b in crx_id for n in (b>>4,b&15)))
