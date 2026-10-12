//! Chrome extension packages (CRX3): check the signatures before anything
//! is unpacked.
//!
//! A CRX3 file is "Cr24", version 3, the header's length, a protobuf header
//! (`CrxFileHeader`), then the zip. The header holds key proofs (an SPKI
//! public key and a signature, RSA or ECDSA, both over SHA-256) and the
//! signed data, whose `crx_id` is the first 16 bytes of the SHA-256 of the
//! extension's own key. As in Chrome, every proof must verify and one must
//! be by the key the id names. The id, as letters a–p, is the extension's
//! Web Store id. CRX2 (unsigned zip offsets, retired by Chrome in 2019) is
//! refused.

use std::ffi::c_void;

type CFRef = *const c_void;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecAttrKeyType: CFRef;
    static kSecAttrKeyTypeRSA: CFRef;
    static kSecAttrKeyTypeECSECPrimeRandom: CFRef;
    static kSecAttrKeyClass: CFRef;
    static kSecAttrKeyClassPublic: CFRef;
    static kSecKeyAlgorithmRSASignatureMessagePKCS1v15SHA256: CFRef;
    static kSecKeyAlgorithmECDSASignatureMessageX962SHA256: CFRef;
    fn SecKeyCreateWithData(data: CFRef, attributes: CFRef, error: *mut CFRef) -> CFRef;
    fn SecKeyVerifySignature(key: CFRef, algorithm: CFRef, data: CFRef, signature: CFRef, error: *mut CFRef) -> u8;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFDataCreate(alloc: CFRef, bytes: *const u8, len: isize) -> CFRef;
    fn CFDictionaryCreate(
        alloc: CFRef,
        keys: *const CFRef,
        values: *const CFRef,
        n: isize,
        key_cb: *const c_void,
        value_cb: *const c_void,
    ) -> CFRef;
    fn CFRelease(cf: CFRef);
}

unsafe extern "C" {
    // CommonCrypto, in libSystem.
    fn CC_SHA256(data: *const c_void, len: u32, md: *mut u8) -> *mut u8;
}

/// A CoreFoundation object, released when dropped.
struct Cf(CFRef);

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

fn data(bytes: &[u8]) -> Cf {
    Cf(unsafe { CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize) })
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut md = [0u8; 32];
    unsafe { CC_SHA256(bytes.as_ptr().cast(), bytes.len() as u32, md.as_mut_ptr()) };
    md
}

#[derive(Clone, Copy)]
enum Alg {
    Rsa,
    Ecdsa,
}

/// Does `signature` by the SPKI key `spki` sign `message`?
fn verifies(alg: Alg, spki: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Some(raw) = spki_key(spki) else { return false };
    unsafe {
        let (ty, algorithm) = match alg {
            Alg::Rsa => (kSecAttrKeyTypeRSA, kSecKeyAlgorithmRSASignatureMessagePKCS1v15SHA256),
            Alg::Ecdsa => (kSecAttrKeyTypeECSECPrimeRandom, kSecKeyAlgorithmECDSASignatureMessageX962SHA256),
        };
        let keys = [kSecAttrKeyType, kSecAttrKeyClass];
        let values = [ty, kSecAttrKeyClassPublic];
        let attrs = Cf(CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            2,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        ));
        let mut error: CFRef = std::ptr::null();
        let key = Cf(SecKeyCreateWithData(data(raw).0, attrs.0, &mut error));
        drop(Cf(error));
        if key.0.is_null() {
            return false;
        }
        let (message, signature) = (data(message), data(signature));
        let mut error: CFRef = std::ptr::null();
        let ok = SecKeyVerifySignature(key.0, algorithm, message.0, signature.0, &mut error) != 0;
        drop(Cf(error));
        ok
    }
}

/// One DER element at the start of `b`: (tag, contents, rest).
fn der(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, b) = b.split_first()?;
    let (&first, mut b) = b.split_first()?;
    let len = if first < 0x80 {
        first as usize
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || b.len() < n {
            return None;
        }
        let len = b[..n].iter().fold(0usize, |l, &x| (l << 8) | x as usize);
        b = &b[n..];
        len
    };
    (b.len() >= len).then(|| (tag, &b[..len], &b[len..]))
}

/// SubjectPublicKeyInfo → the key inside it (PKCS#1 for RSA, the X9.63 point
/// for EC), which is what `SecKeyCreateWithData` takes.
fn spki_key(spki: &[u8]) -> Option<&[u8]> {
    let (0x30, body, _) = der(spki)? else { return None };
    let (0x30, _alg, rest) = der(body)? else { return None };
    let (0x03, bits, _) = der(rest)? else { return None };
    bits.strip_prefix(&[0])
}

/// Protobuf fields of `b`: (field number, bytes) for length-delimited ones;
/// varints are skipped. None if malformed.
fn fields(mut b: &[u8]) -> Option<Vec<(u64, &[u8])>> {
    fn varint(b: &mut &[u8]) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&x, rest) = b.split_first()?;
            *b = rest;
            v |= ((x & 0x7f) as u64) << shift;
            if x & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    let mut out = Vec::new();
    while !b.is_empty() {
        let key = varint(&mut b)?;
        match key & 7 {
            0 => {
                varint(&mut b)?;
            }
            2 => {
                let len = usize::try_from(varint(&mut b)?).ok()?;
                if b.len() < len {
                    return None;
                }
                out.push((key >> 3, &b[..len]));
                b = &b[len..];
            }
            _ => return None,
        }
    }
    Some(out)
}

/// The id for a key: SHA-256 of its SPKI, first 16 bytes, as letters a–p.
fn id_of(crx_id: &[u8]) -> String {
    crx_id.iter().flat_map(|b| [b >> 4, b & 15]).map(|n| (b'a' + n) as char).collect()
}

/// Check a CRX3 package: (its zip, its extension id). Err says why not.
pub fn verify(bytes: &[u8]) -> Result<(&[u8], String), String> {
    let bad = |why: &str| format!("This package can't be trusted: {why}.");
    let word = |i: usize| bytes.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    match word(4) {
        Some(3) => {}
        Some(2) => return Err(bad("it uses the old CRX2 format, which has no signed id")),
        _ => return Err(bad("unknown package version")),
    }
    let len = word(8).ok_or_else(|| bad("truncated"))?;
    let header = bytes.get(12..12 + len).ok_or_else(|| bad("truncated"))?;
    let zip = &bytes[12 + len..];
    if !zip.starts_with(b"PK\x03\x04") {
        return Err("That isn't an extension package (.crx or .zip).".into());
    }
    let header = fields(header).ok_or_else(|| bad("malformed header"))?;
    let signed = header.iter().find(|(f, _)| *f == 10000).map(|(_, d)| *d).ok_or_else(|| bad("no signed data"))?;
    let crx_id = fields(signed)
        .and_then(|f| f.iter().find(|(n, _)| *n == 1).map(|(_, d)| *d))
        .filter(|id| id.len() == 16)
        .ok_or_else(|| bad("no id"))?;
    let mut message = b"CRX3 SignedData\x00".to_vec();
    message.extend_from_slice(&(signed.len() as u32).to_le_bytes());
    message.extend_from_slice(signed);
    message.extend_from_slice(zip);
    let mut own_key = false;
    let mut proofs = 0;
    for (field, proof) in &header {
        let alg = match field {
            2 => Alg::Rsa,
            3 => Alg::Ecdsa,
            _ => continue,
        };
        let parts = fields(proof).ok_or_else(|| bad("malformed signature"))?;
        let get = |n| parts.iter().find(|(f, _)| *f == n).map(|(_, d)| *d).unwrap_or_default();
        let (key, signature) = (get(1), get(2));
        if !verifies(alg, key, &message, signature) {
            return Err(bad("a signature doesn't match its contents"));
        }
        proofs += 1;
        own_key |= sha256(key)[..16] == *crx_id;
    }
    if proofs == 0 || !own_key {
        return Err(bad("it isn't signed by the key its id names"));
    }
    Ok((zip, id_of(crx_id)))
}

#[cfg(test)]
mod tests {
    use super::verify;

    const SIGNED: &[u8] = include_bytes!("../tests/fixtures/title.crx");
    /// The fixture's id (made by tests/fixtures/make-crx.py; the key is gone).
    const ID: &str = include_str!("../tests/fixtures/title.crx.id");

    #[test]
    fn signed_packages_verify_and_give_their_id() {
        let (zip, id) = verify(SIGNED).unwrap();
        assert!(zip.starts_with(b"PK\x03\x04"));
        assert_eq!(id, ID.trim());
    }

    #[test]
    fn changed_or_unsigned_packages_are_refused() {
        // One byte of the zip changed.
        let mut changed = SIGNED.to_vec();
        let last = changed.len() - 30;
        changed[last] ^= 1;
        assert!(verify(&changed).unwrap_err().contains("doesn't match"));
        // No signatures at all (the header is only the signed data).
        let mut bare = b"Cr24\x03\x00\x00\x00\x16\x00\x00\x00".to_vec();
        bare.extend_from_slice(b"\x82\xf1\x04\x12\x0a\x10");
        bare.extend_from_slice(&[7u8; 16]);
        bare.extend_from_slice(b"PK\x03\x04rest");
        assert!(verify(&bare).unwrap_err().contains("isn't signed"));
        let mut crx2 = b"Cr24\x02\x00\x00\x00\x01\x00\x00\x00\x02\x00\x00\x00kssPK\x03\x04".to_vec();
        crx2.push(0);
        assert!(verify(&crx2).unwrap_err().contains("CRX2"));
        assert!(verify(b"Cr24\x03\x00\x00\x00\xff\xff\xff\x00").is_err(), "header past the end");
    }
}
