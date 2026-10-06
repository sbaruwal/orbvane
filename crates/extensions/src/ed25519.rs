//! Ed25519 signature verification (RFC 8032) and the SHA-512 it needs, for the signatures Open
//! VSX publishes with each package (`files.signature`: a zip holding `.signature.sig`, the
//! signature of the `.vsix` bytes; `files.publicKey`: the registry's key as PEM). Field
//! arithmetic in sixteen 16-bit limbs, after the well-known compact reference implementation;
//! only verification, so nothing here handles secrets.

use std::io::Read;

// ------------------------------------------------------------------ SHA-512

const K: [u64; 80] = [
    0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc, 0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
    0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2, 0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694,
    0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65, 0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
    0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4, 0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70,
    0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df, 0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b,
    0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30, 0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8, 0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec, 0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b,
    0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178, 0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
    0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c, 0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
];

/// SHA-512, fed in pieces.
pub struct Sha512 {
    h: [u64; 8],
    block: [u8; 128],
    filled: usize,
    total: u128,
}

impl Default for Sha512 {
    fn default() -> Self {
        Sha512 {
            h: [0x6a09e667f3bcc908, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1, 0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179],
            block: [0; 128],
            filled: 0,
            total: 0,
        }
    }
}

impl Sha512 {
    fn compress(h: &mut [u64; 8], block: &[u8]) {
        let mut w = [0u64; 80];
        for (i, c) in block.chunks_exact(8).enumerate() {
            w[i] = u64::from_be_bytes(c.try_into().unwrap());
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
        for i in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(v);
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u128;
        if self.filled > 0 {
            let n = (128 - self.filled).min(data.len());
            self.block[self.filled..self.filled + n].copy_from_slice(&data[..n]);
            self.filled += n;
            data = &data[n..];
            if self.filled < 128 {
                return;
            }
            Self::compress(&mut self.h, &self.block);
            self.filled = 0;
        }
        let mut chunks = data.chunks_exact(128);
        for c in &mut chunks {
            Self::compress(&mut self.h, c);
        }
        let rest = chunks.remainder();
        self.block[..rest.len()].copy_from_slice(rest);
        self.filled = rest.len();
    }

    pub fn finish(mut self) -> [u8; 64] {
        let bits = self.total * 8;
        let mut pad = vec![0x80u8];
        let len = (self.filled + 1) % 128;
        pad.resize(1 + if len <= 112 { 112 - len } else { 240 - len }, 0);
        pad.extend_from_slice(&bits.to_be_bytes());
        let total = self.total;
        self.update(&pad);
        self.total = total;
        let mut out = [0u8; 64];
        for (o, h) in out.chunks_exact_mut(8).zip(self.h) {
            o.copy_from_slice(&h.to_be_bytes());
        }
        out
    }
}

// ------------------------------------------------------------------ the curve

/// A field element mod 2^255 - 19, in sixteen 16-bit limbs (allowed to grow between carries).
type Gf = [i64; 16];

const GF0: Gf = [0; 16];
const GF1: Gf = [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
/// The curve's d, and 2d.
const D: Gf = [0x78a3, 0x1359, 0x4dca, 0x75eb, 0xd8ab, 0x4141, 0x0a4d, 0x0070, 0xe898, 0x7779, 0x4079, 0x8cc7, 0xfe73, 0x2b6f, 0x6cee, 0x5203];
const D2: Gf = [0xf159, 0x26b2, 0x9b94, 0xebd6, 0xb156, 0x8283, 0x149a, 0x00e0, 0xd130, 0xeef3, 0x80f2, 0x198e, 0xfce7, 0x56df, 0xd9dc, 0x2406];
/// The base point's coordinates.
const X: Gf = [0xd51a, 0x8f25, 0x2d60, 0xc956, 0xa7b2, 0x9525, 0xc760, 0x692c, 0xdc5c, 0xfdd6, 0xe231, 0xc0a4, 0x53fe, 0xcd6e, 0x36d3, 0x2169];
const Y: Gf = [0x6658, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666, 0x6666];
/// sqrt(-1).
const I: Gf = [0xa0b0, 0x4a0e, 0x1b27, 0xc4ee, 0xe478, 0xad2f, 0x1806, 0x2f43, 0xd7a7, 0x3dfb, 0x0099, 0x2b4d, 0xdf0b, 0x4fc1, 0x2480, 0x2b83];
/// The group order, little-endian.
const L: [i64; 32] = [0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10];

fn carry(o: &mut Gf) {
    for i in 0..16 {
        o[i] += 1 << 16;
        let c = o[i] >> 16;
        if i < 15 {
            o[i + 1] += c - 1;
        } else {
            o[0] += 38 * (c - 1);
        }
        o[i] -= c << 16;
    }
}

/// Swaps `p` and `q` when `b` is 1 (without branching on it).
fn select(p: &mut Gf, q: &mut Gf, b: i64) {
    let c = !(b - 1);
    for i in 0..16 {
        let t = c & (p[i] ^ q[i]);
        p[i] ^= t;
        q[i] ^= t;
    }
}

fn pack25519(n: &Gf) -> [u8; 32] {
    let mut t = *n;
    carry(&mut t);
    carry(&mut t);
    carry(&mut t);
    for _ in 0..2 {
        let mut m = GF0;
        m[0] = t[0] - 0xffed;
        for i in 1..15 {
            m[i] = t[i] - 0xffff - ((m[i - 1] >> 16) & 1);
            m[i - 1] &= 0xffff;
        }
        m[15] = t[15] - 0x7fff - ((m[14] >> 16) & 1);
        let b = (m[15] >> 16) & 1;
        m[14] &= 0xffff;
        select(&mut t, &mut m, 1 - b);
    }
    let mut o = [0u8; 32];
    for i in 0..16 {
        o[2 * i] = (t[i] & 0xff) as u8;
        o[2 * i + 1] = (t[i] >> 8) as u8;
    }
    o
}

fn differ(a: &Gf, b: &Gf) -> bool {
    pack25519(a) != pack25519(b)
}

fn parity(a: &Gf) -> u8 {
    pack25519(a)[0] & 1
}

fn unpack25519(n: &[u8; 32]) -> Gf {
    let mut o = GF0;
    for i in 0..16 {
        o[i] = n[2 * i] as i64 + ((n[2 * i + 1] as i64) << 8);
    }
    o[15] &= 0x7fff;
    o
}

fn add(a: &Gf, b: &Gf) -> Gf {
    std::array::from_fn(|i| a[i] + b[i])
}

fn sub(a: &Gf, b: &Gf) -> Gf {
    std::array::from_fn(|i| a[i] - b[i])
}

fn mul(a: &Gf, b: &Gf) -> Gf {
    let mut t = [0i64; 31];
    for i in 0..16 {
        for j in 0..16 {
            t[i + j] += a[i] * b[j];
        }
    }
    for i in 0..15 {
        t[i] += 38 * t[i + 16];
    }
    let mut o = GF0;
    o.copy_from_slice(&t[..16]);
    carry(&mut o);
    carry(&mut o);
    o
}

fn square(a: &Gf) -> Gf {
    mul(a, a)
}

fn invert(i: &Gf) -> Gf {
    let mut c = *i;
    for a in (0..=253).rev() {
        c = square(&c);
        if a != 2 && a != 4 {
            c = mul(&c, i);
        }
    }
    c
}

/// i^((p-5)/8).
fn pow2523(i: &Gf) -> Gf {
    let mut c = *i;
    for a in (0..=250).rev() {
        c = square(&c);
        if a != 1 {
            c = mul(&c, i);
        }
    }
    c
}

/// A point in extended coordinates (X, Y, Z, T).
type Point = [Gf; 4];

fn point_add(p: &mut Point, q: &Point) {
    let a = mul(&sub(&p[1], &p[0]), &sub(&q[1], &q[0]));
    let b = mul(&add(&p[0], &p[1]), &add(&q[0], &q[1]));
    let c = mul(&mul(&p[3], &q[3]), &D2);
    let d = mul(&p[2], &q[2]);
    let d = add(&d, &d);
    let (e, f, g, h) = (sub(&b, &a), sub(&d, &c), add(&d, &c), add(&b, &a));
    p[0] = mul(&e, &f);
    p[1] = mul(&h, &g);
    p[2] = mul(&g, &f);
    p[3] = mul(&e, &h);
}

fn cswap(p: &mut Point, q: &mut Point, b: i64) {
    for i in 0..4 {
        select(&mut p[i], &mut q[i], b);
    }
}

fn pack(p: &Point) -> [u8; 32] {
    let zi = invert(&p[2]);
    let (tx, ty) = (mul(&p[0], &zi), mul(&p[1], &zi));
    let mut r = pack25519(&ty);
    r[31] ^= parity(&tx) << 7;
    r
}

/// `s` times `q`.
fn scalarmult(q: &Point, s: &[u8; 32]) -> Point {
    let mut p: Point = [GF0, GF1, GF1, GF0];
    let mut q = *q;
    for i in (0..256).rev() {
        let b = ((s[i / 8] >> (i & 7)) & 1) as i64;
        cswap(&mut p, &mut q, b);
        let pp = p;
        point_add(&mut q, &pp);
        point_add(&mut p, &pp);
        cswap(&mut p, &mut q, b);
    }
    p
}

fn scalarbase(s: &[u8; 32]) -> Point {
    scalarmult(&[X, Y, GF1, mul(&X, &Y)], s)
}

/// The negated point encoded in `p` (None: not a point).
fn unpack_negated(p: &[u8; 32]) -> Option<Point> {
    let mut r: Point = [GF0, unpack25519(p), GF1, GF0];
    let num = square(&r[1]);
    let den = mul(&num, &D);
    let num = sub(&num, &r[2]);
    let den = add(&r[2], &den);
    let den2 = square(&den);
    let den4 = square(&den2);
    let den6 = mul(&den4, &den2);
    let mut t = mul(&mul(&den6, &num), &den);
    t = pow2523(&t);
    t = mul(&mul(&mul(&t, &num), &den), &den);
    r[0] = mul(&t, &den);
    if differ(&mul(&square(&r[0]), &den), &num) {
        r[0] = mul(&r[0], &I);
    }
    if differ(&mul(&square(&r[0]), &den), &num) {
        return None;
    }
    if parity(&r[0]) == p[31] >> 7 {
        r[0] = sub(&GF0, &r[0]);
    }
    r[3] = mul(&r[0], &r[1]);
    Some(r)
}

/// A 512-bit little-endian number mod L.
fn reduce(h: &[u8; 64]) -> [u8; 32] {
    let mut x: [i64; 64] = std::array::from_fn(|i| h[i] as i64);
    for i in (32..64).rev() {
        let mut c = 0;
        let mut j = i - 32;
        while j < i - 12 {
            x[j] += c - 16 * x[i] * L[j - (i - 32)];
            c = (x[j] + 128) >> 8;
            x[j] -= c << 8;
            j += 1;
        }
        x[j] += c;
        x[i] = 0;
    }
    let mut c = 0;
    for j in 0..32 {
        x[j] += c - (x[31] >> 4) * L[j];
        c = x[j] >> 8;
        x[j] &= 255;
    }
    for j in 0..32 {
        x[j] -= c * L[j];
    }
    let mut r = [0u8; 32];
    for i in 0..32 {
        x[i + 1] += x[i] >> 8;
        r[i] = (x[i] & 255) as u8;
    }
    r
}

/// Whether the scalar `s` (little-endian) is below the group order.
fn below_order(s: &[u8]) -> bool {
    for i in (0..32).rev() {
        match (s[i] as i64).cmp(&L[i]) {
            std::cmp::Ordering::Less => return true,
            std::cmp::Ordering::Greater => return false,
            std::cmp::Ordering::Equal => {}
        }
    }
    false
}

/// Whether `signature` is `public_key`'s signature of the bytes `message` reads.
pub fn verify_reader(public_key: &[u8; 32], signature: &[u8; 64], message: &mut dyn Read) -> std::io::Result<bool> {
    let (r, s) = signature.split_at(32);
    let Some(a) = unpack_negated(public_key).filter(|_| below_order(s)) else { return Ok(false) };
    let mut hash = Sha512::default();
    hash.update(r);
    hash.update(public_key);
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = message.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    let h = reduce(&hash.finish());
    // [s]B - [h]A must be R.
    let mut p = scalarmult(&a, &h);
    point_add(&mut p, &scalarbase(s.try_into().unwrap()));
    Ok(pack(&p) == r)
}

pub fn verify(public_key: &[u8; 32], signature: &[u8; 64], message: &[u8]) -> bool {
    verify_reader(public_key, signature, &mut &message[..]).unwrap_or(false)
}

fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace()) {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The key in an Ed25519 public key PEM (`-----BEGIN PUBLIC KEY-----`).
pub fn key_from_pem(pem: &str) -> Option<[u8; 32]> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = base64(&body)?;
    // SubjectPublicKeyInfo: SEQUENCE { SEQUENCE { OID 1.3.101.112 }, BIT STRING (0 unused) key }.
    const PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
    der.strip_prefix(&PREFIX[..])?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn sha512_hex(data: &[u8]) -> String {
        let mut h = Sha512::default();
        h.update(data);
        h.finish().iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn hashes() {
        assert_eq!(sha512_hex(b"abc"), "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f");
        assert_eq!(sha512_hex(b""), "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e");
        // In pieces of every size, against the system's shasum.
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 + i / 13) as u8).collect();
        let dir = std::env::temp_dir().join(format!("orbvane-sha512-{}", std::process::id()));
        std::fs::write(&dir, &data).unwrap();
        let out = std::process::Command::new("/usr/bin/shasum").args(["-a", "512"]).arg(&dir).output().unwrap();
        let _ = std::fs::remove_file(&dir);
        let want = String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap().to_string();
        for step in [1, 7, 127, 128, 129, 1000] {
            let mut h = Sha512::default();
            for c in data.chunks(step) {
                h.update(c);
            }
            let got: String = h.finish().iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(got, want, "pieces of {step}");
        }
    }

    #[test]
    fn verifies_the_rfc_examples() {
        let cases = [
            ("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a", "", "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"),
            ("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c", "72", "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"),
        ];
        for (key, msg, sig) in cases {
            let (key, msg, sig): ([u8; 32], Vec<u8>, [u8; 64]) = (hex(key).try_into().unwrap(), hex(msg), hex(sig).try_into().unwrap());
            assert!(verify(&key, &sig, &msg));
            // Any change is caught.
            let mut other = msg.clone();
            other.push(0);
            assert!(!verify(&key, &sig, &other));
            let mut bad = sig;
            bad[5] ^= 1;
            assert!(!verify(&key, &bad, &msg));
            let mut bad = sig;
            bad[40] ^= 1;
            assert!(!verify(&key, &bad, &msg));
        }
    }

    #[test]
    fn reads_pem_keys() {
        let pem = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAje+vAaSS1zHV5WHCJSa5UXvxRo6+yerEU3IEmtuEuF4=\n-----END PUBLIC KEY-----\n";
        let key = key_from_pem(pem).unwrap();
        assert_eq!(&key[..4], &[0x8d, 0xef, 0xaf, 0x01]);
        assert!(key_from_pem("-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----").is_none());
    }
}
