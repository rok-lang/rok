#!/usr/bin/env python3
# Regenerates src/vectors.txt, the known-answer subset the crypto KAT case
# runs, from the official vector files (downloaded into <dir> when
# missing; they are untrusted input and only parsed as data):
#
#   python3 -I gen_vectors.py <dir> [--full <file>]
#
# --full also writes every converted case (about 16 000) for a one-off run
# of the same driver on a file (src/main.rok with include_str! replaced
# by a read of argv[1]).
#
# NIST CAVP (csrc.nist.gov/.../Cryptographic-Algorithm-Validation-Program/
# documents/), SHA-256 of the zips used:
#   shs/shabytetestvectors.zip      929ef80b7b3418aca026643f6f248815913b60e01741a44bba9e118067f4c9b8
#   sha3/sha-3bytetestvectors.zip   cd07701af2e47f5cc889d642528b4bf11f8b6eb55797c7307a96828ed8d8fc8c
#   sha3/shakebytetestvectors.zip   debfebc3157b3ceea002b84ca38476420389a3bf7e97dc5f53ea4689a16de4c7
#   mac/hmactestvectors.zip         418c3837d38f249d6668146bd0090db24dd3c02d2e6797e3de33860a387ae4bd
#   mac/gcmtestvectors.zip          f9fc479e134cde2980b3bb7cddbcb567b2cd96fd753835243ed067699f26a023
#   dss/186-4ecdsatestvectors.zip   fe47cc92b4cee418236125c9ffbcd9bb01c8c34e74a4ba195d954bcb72824752
#   dss/186-3rsatestvectors.zip     8405aeb3572a4f98ed4b1a3ccb3f2f49e725462dd28ec4759d6a15d88855d19c
# Wycheproof: github.com/C2SP/wycheproof testvectors_v1 at the commit in
# WYCHEPROOF.
#
# Kept: SHA-256/384/512 and SHA3-256/512 (short, long, Monte Carlo),
# SHAKE128/256 (short, long, variable output), HMAC-SHA-2 (truncated tags
# as prefixes), AES-128/256-GCM with 96-bit IVs and 128-bit tags,
# ChaCha20-Poly1305, X25519, Ed25519, ECDSA P-256/P-384 with SHA-2 (CAVP
# SigVer, SigGen and KeyPair; Wycheproof DER), RSA 2048-4096 PKCS #1 v1.5
# and PSS with MGF1 of the message hash (verification; signing where it
# is deterministic: PKCS #1 v1.5, PSS with a salt as long as the hash;
# CAVP private keys rebuilt from n, e, d), HKDF, ML-KEM-768. Other sizes,
# hashes and parameters are skipped; AEAD cases with other nonce lengths
# are kept as cases that must be refused. The subset takes each distinct
# Wycheproof (flags, result) pair first, then evenly spaced cases.
import base64, collections, hashlib, json, math, os, random, re, sys, urllib.request, zipfile

WYCHEPROOF = 'https://raw.githubusercontent.com/C2SP/wycheproof/7a325932231bd7a724c791c8b0d65ea4d6d2f435/testvectors_v1/'
CAVP = 'https://csrc.nist.gov/CSRC/media/Projects/Cryptographic-Algorithm-Validation-Program/documents/'
ZIPS = ['shs/shabytetestvectors.zip', 'sha3/sha-3bytetestvectors.zip', 'sha3/shakebytetestvectors.zip',
        'mac/hmactestvectors.zip', 'mac/gcmtestvectors.zip', 'dss/186-4ecdsatestvectors.zip',
        'dss/186-3rsatestvectors.zip']
NAMES = '''aes_gcm_test chacha20_poly1305_test x25519_test ed25519_test
ecdsa_secp256r1_sha256_test ecdsa_secp256r1_sha512_test ecdsa_secp384r1_sha384_test
ecdsa_secp384r1_sha256_test ecdsa_secp384r1_sha512_test
hkdf_sha256_test hkdf_sha384_test hkdf_sha512_test
hmac_sha256_test hmac_sha384_test hmac_sha512_test
rsa_signature_2048_sha256_test rsa_signature_2048_sha384_test rsa_signature_2048_sha512_test
rsa_signature_3072_sha256_test rsa_signature_3072_sha384_test rsa_signature_3072_sha512_test
rsa_signature_4096_sha256_test rsa_signature_4096_sha384_test rsa_signature_4096_sha512_test
rsa_pkcs1_2048_sig_gen_test rsa_pkcs1_3072_sig_gen_test rsa_pkcs1_4096_sig_gen_test
rsa_pss_2048_sha256_mgf1_0_test rsa_pss_2048_sha256_mgf1_32_test rsa_pss_2048_sha384_mgf1_48_test
rsa_pss_3072_sha256_mgf1_32_test rsa_pss_4096_sha256_mgf1_32_test rsa_pss_4096_sha384_mgf1_48_test
rsa_pss_4096_sha512_mgf1_32_test rsa_pss_4096_sha512_mgf1_64_test rsa_pss_misc_test
mlkem_768_test mlkem_768_encaps_test mlkem_768_keygen_seed_test mlkem_768_semi_expanded_decaps_test'''.split()

dl = sys.argv[1]
full_out = sys.argv[sys.argv.index('--full') + 1] if '--full' in sys.argv else None
sub_out = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'src', 'vectors.txt')
os.makedirs(os.path.join(dl, 'wp'), exist_ok=True)
for z in ZIPS:
    p = os.path.join(dl, os.path.basename(z))
    if not os.path.exists(p):
        with urllib.request.urlopen(CAVP + z, timeout=120) as r:
            open(p, 'wb').write(r.read())
for n in NAMES:
    p = os.path.join(dl, 'wp', n + '.json')
    if not os.path.exists(p):
        with urllib.request.urlopen(WYCHEPROOF + n + '.json', timeout=120) as r:
            open(p, 'wb').write(r.read())
H = {'SHA256': 'sha256', 'SHA384': 'sha384', 'SHA512': 'sha512',
     'SHA-256': 'sha256', 'SHA-384': 'sha384', 'SHA-512': 'sha512'}
HLEN = {'sha256': 32, 'sha384': 48, 'sha512': 64}
suites = collections.OrderedDict()   # suite -> list of (key, line)
defs = {}                            # hex -> name
def_lines = collections.OrderedDict()


def h(x):
    return x if x else '-'


def ref(hexstr):
    # Long repeated fields (keys) become `@name`.
    if len(hexstr) < 200:
        return h(hexstr)
    name = defs.get(hexstr)
    if name is None:
        name = 'k%d' % len(defs)
        defs[hexstr] = name
        def_lines[name] = hexstr
    return '@' + name


def add(suite, key, *fields):
    suites.setdefault(suite, []).append((key, ' '.join([suite] + [h(x) for x in fields])))


def rsp_blocks(text):
    # CAVP .rsp: yields (section headers dict, entry dict) per blank-line
    # separated entry; header lines `[k = v]` or `[k]` persist.
    hdr = {}
    cur = {}
    for raw in text.splitlines() + ['']:
        line = raw.strip()
        if line.startswith('#'):
            continue
        if line.startswith('['):
            if cur:
                yield dict(hdr), cur
                cur = {}
            for part in line.strip('[]').split(','):
                if '=' in part:
                    k, v = part.split('=', 1)
                    hdr[k.strip()] = v.strip()
                else:
                    hdr[part.strip()] = ''
            continue
        if not line:
            if cur:
                yield dict(hdr), cur
                cur = {}
            continue
        if '=' in line:
            k, v = line.split('=', 1)
            cur[k.strip()] = v.strip()
        else:
            cur[line] = ''


def zread(zname, member):
    z = zipfile.ZipFile(os.path.join(dl, zname))
    for n in z.namelist():
        if n.endswith('/' + member) or n == member:
            return z.read(n).decode('latin1')
    raise KeyError(member)


def msg_of(e):
    n = int(e.get('Len', '-1'))
    m = e['Msg'].lower()
    return '' if n == 0 else m


# CAVP SHA-2 / SHA-3 / SHAKE.
for alg, name in [('sha256', 'SHA256'), ('sha384', 'SHA384'), ('sha512', 'SHA512')]:
    for kind in ['ShortMsg', 'LongMsg']:
        for _, e in rsp_blocks(zread('shabytetestvectors.zip', f'{name}{kind}.rsp')):
            if 'MD' in e:
                add(f'cavp-{alg}', None, 'hash', alg, h(msg_of(e)), e['MD'].lower())
    seed = None
    for hd, e in rsp_blocks(zread('shabytetestvectors.zip', f'{name}Monte.rsp')):
        if 'Seed' in e:
            seed = e['Seed'].lower()
        if 'MD' in e:
            add(f'cavp-{alg}-monte', None, 'monte', alg, seed, e['MD'].lower())
            seed = e['MD'].lower()
for alg, name in [('sha3-256', 'SHA3_256'), ('sha3-512', 'SHA3_512')]:
    for kind in ['ShortMsg', 'LongMsg']:
        for _, e in rsp_blocks(zread('sha-3bytetestvectors.zip', f'{name}{kind}.rsp')):
            if 'MD' in e:
                add(f'cavp-{alg}', None, 'hash', alg, h(msg_of(e)), e['MD'].lower())
    seed = None
    for _, e in rsp_blocks(zread('sha-3bytetestvectors.zip', f'{name}Monte.rsp')):
        if 'Msg' in e and 'MD' not in e:
            seed = e['Msg'].lower()
        if 'Seed' in e:
            seed = e['Seed'].lower()
        if 'MD' in e:
            add(f'cavp-{alg}-monte', None, 'monte', alg, seed, e['MD'].lower())
            seed = e['MD'].lower()
for alg, name in [('shake128', 'SHAKE128'), ('shake256', 'SHAKE256')]:
    for kind in ['ShortMsg', 'LongMsg']:
        for _, e in rsp_blocks(zread('shakebytetestvectors.zip', f'{name}{kind}.rsp')):
            if 'Output' in e:
                out = e['Output'].lower()
                add(f'cavp-{alg}', None, 'shake', alg, h(msg_of(e)), str(len(out) // 2), out)
    for _, e in rsp_blocks(zread('shakebytetestvectors.zip', f'{name}VariableOut.rsp')):
        if 'Output' in e:
            out = e['Output'].lower()
            add(f'cavp-{alg}-varout', None, 'shake', alg, e['Msg'].lower(), str(len(out) // 2), out)

# CAVP HMAC.
for hd, e in rsp_blocks(zread('hmactestvectors.zip', 'HMAC.rsp')):
    alg = {'32': 'sha256', '48': 'sha384', '64': 'sha512'}.get(hd.get('L'))
    if alg and 'Mac' in e:
        add(f'cavp-hmac-{alg}', None, 'hmac', alg, e['Key'].lower(), e['Msg'].lower(), e['Mac'].lower(), '1')

# CAVP AES-GCM (96-bit IV, 128-bit tag).
for kl, alg in [('128', 'aes128gcm'), ('256', 'aes256gcm')]:
    for hd, e in rsp_blocks(zread('gcmtestvectors.zip', f'gcmEncryptExtIV{kl}.rsp')):
        if hd.get('IVlen') == '96' and hd.get('Taglen') == '128' and 'Tag' in e:
            add(f'cavp-gcm-enc-{kl}', None, 'aead', alg, e['Key'], e['IV'], h(e['AAD']), h(e['PT']), h(e['CT']), e['Tag'], '1')
    for hd, e in rsp_blocks(zread('gcmtestvectors.zip', f'gcmDecrypt{kl}.rsp')):
        if hd.get('IVlen') == '96' and hd.get('Taglen') == '128' and 'Tag' in e:
            ok = 'FAIL' not in e
            add(f'cavp-gcm-dec-{kl}', None, 'aead', alg, e['Key'], e['IV'], h(e['AAD']), h(e.get('PT', '')), h(e['CT']), e['Tag'], '1' if ok else '0')

# CAVP ECDSA.
CURVES = {'P-256': ('p256', 32), 'P-384': ('p384', 48)}


def der_int(v):
    b = v.to_bytes(max(1, (v.bit_length() + 7) // 8), 'big')
    if b[0] & 0x80:
        b = b'\0' + b
    return b'\x02' + bytes([len(b)]) + b


def der_sig(r, s):
    body = der_int(r) + der_int(s)
    return (b'\x30' + (bytes([0x81]) if len(body) >= 0x80 else b'') + bytes([len(body)]) + body).hex()


def sec1(qx, qy, size):
    return '04' + qx.rjust(2 * size, '0') + qy.rjust(2 * size, '0')


z = '186-4ecdsatestvectors.zip'
for text_name in ['SigVer.rsp', 'SigGen.rsp', 'KeyPair.rsp']:
    text = zread(z, text_name)
    curve = hashn = None
    cur = {}
    for raw in text.splitlines() + ['']:
        line = raw.strip()
        if line.startswith('#'):
            continue
        m = re.match(r'^\[(P-\d+|[BK]-\d+)(?:,(SHA-\d+))?\]$', line)
        if m:
            curve, hashn = m.group(1), m.group(2)
            continue
        if '=' in line:
            k, v = line.split('=', 1)
            cur[k.strip()] = v.strip()
            if text_name == 'SigVer.rsp' and k.strip() != 'Result':
                continue
            if text_name == 'SigGen.rsp' and k.strip() != 'S':
                continue
            if text_name == 'KeyPair.rsp' and k.strip() != 'Qy':
                continue
        else:
            continue
        e = cur
        cur = {}
        if curve not in CURVES:
            continue
        cname, size = CURVES[curve]
        if text_name == 'KeyPair.rsp':
            add(f'cavp-ecdsa-keypair-{cname}', None, 'ecdsa-pub', cname, e['d'], sec1(e['Qx'], e['Qy'], size))
            continue
        alg = H.get((hashn or '').replace('-', ''))
        if alg is None:
            continue
        sig = der_sig(int(e['R'], 16), int(e['S'], 16))
        pk = sec1(e['Qx'], e['Qy'], size)
        if text_name == 'SigVer.rsp':
            add(f'cavp-ecdsa-sigver-{cname}-{alg}', None, 'ecdsa', cname, alg, pk, e['Msg'], sig, '1' if e['Result'].startswith('P') else '0')
        else:
            add(f'cavp-ecdsa-siggen-{cname}-{alg}', None, 'ecdsa', cname, alg, pk, e['Msg'], sig, '1')
            if 'd' in e:
                add(f'cavp-ecdsa-siggen-{cname}-{alg}', None, 'ecdsa-pub', cname, e['d'], pk)

# CAVP RSA.


def factor(n, e, d):
    k = d * e - 1
    t = 0
    r = k
    while r % 2 == 0:
        r //= 2
        t += 1
    rng = random.Random(1)
    while True:
        g = rng.randrange(2, n - 1)
        y = pow(g, r, n)
        if y in (1, n - 1):
            continue
        for _ in range(t):
            x = pow(y, 2, n)
            if x == 1:
                p = math.gcd(y - 1, n)
                return max(p, n // p), min(p, n // p)
            if x == n - 1:
                break
            y = x


def der_len(n):
    if n < 0x80:
        return bytes([n])
    b = n.to_bytes((n.bit_length() + 7) // 8, 'big')
    return bytes([0x80 | len(b)]) + b


def der_uint(v):
    b = v.to_bytes(max(1, (v.bit_length() + 7) // 8), 'big')
    if b[0] & 0x80:
        b = b'\0' + b
    return b'\x02' + der_len(len(b)) + b


def rsa_priv_der(n, e, d):
    p, q = factor(n, e, d)
    body = b''.join(der_uint(v) for v in [0, n, e, d, p, q, d % (p - 1), d % (q - 1), pow(q, -1, p)])
    return (b'\x30' + der_len(len(body)) + body).hex()


def strip0(x):
    return x.lstrip('0') or '0'


def norm_hex(x):
    x = strip0(x.lower())
    return x if len(x) % 2 == 0 else '0' + x


z = '186-3rsatestvectors.zip'
for name, scheme in [('SigVer15_186-3.rsp', 'pkcs1'), ('SigVerPSS_186-3.rsp', 'pss')]:
    n = None
    for hd, e in rsp_blocks(zread(z, name)):
        if 'n' in e:
            n = e['n'].lower()
        if 'S' not in e or 'Result' not in e:
            continue
        alg = H.get(e.get('SHAAlg'))
        if alg is None or int(hd.get('mod', '0')) < 2048:
            continue
        salt = str(len(e.get('SaltVal', '')) // 2) if scheme == 'pss' and e.get('SaltVal', '00') != '00' else '0'
        add(f'cavp-rsa-sigver-{scheme}', None, 'rsa', scheme, alg, ref(norm_hex(n)), norm_hex(e['e']), e['Msg'].lower(), e['S'].lower().rjust(len(n), '0'), salt, '1' if e['Result'].startswith('P') else '0')
for name, scheme in [('SigGen15_186-3.txt', 'pkcs1'), ('SigGenPSS_186-3.txt', 'pss')]:
    n = ee = d = None
    der = None
    for hd, e in rsp_blocks(zread(z, name)):
        if 'n' in e:
            n = int(e['n'], 16)
        if 'e' in e:
            ee = int(e['e'], 16)
        if 'd' in e:
            d = int(e['d'], 16)
            der = rsa_priv_der(n, ee, d)
        if 'S' not in e:
            continue
        alg = H.get(e.get('SHAAlg'))
        if alg is None:
            continue
        nh = norm_hex('%x' % n)
        eh = norm_hex('%x' % ee)
        salt = e.get('SaltVal', '')
        sl = str(len(salt) // 2) if scheme == 'pss' and salt != '00' else '0'
        sig = e['S'].lower().rjust(len(nh), '0')
        add(f'cavp-rsa-siggen-{scheme}', None, 'rsa', scheme, alg, ref(nh), eh, e['Msg'].lower(), sig, sl, '1')
        if der is not None and (scheme == 'pkcs1' or len(salt) // 2 == HLEN[alg]):
            add(f'cavp-rsa-siggen-{scheme}', None, 'rsa-sign', scheme, alg, ref(der), e['Msg'].lower(), h(salt if scheme == 'pss' else ''), sig)

# Wycheproof.


def wp(name):
    return json.load(open(os.path.join(dl, 'wp', name + '.json')))


def res(t):
    return {'valid': '1', 'invalid': '0', 'acceptable': 'a'}[t['result']]


def key_of(t):
    return ','.join(sorted(t.get('flags', []))) + ':' + t['result']


for name, alg_of in [('aes_gcm_test', lambda g: {128: 'aes128gcm', 256: 'aes256gcm'}.get(g['keySize'])),
                     ('chacha20_poly1305_test', lambda g: 'chacha20poly1305')]:
    for g in wp(name)['testGroups']:
        alg = alg_of(g)
        if alg is None or g['tagSize'] != 128:
            continue
        suite = 'wp-' + name.replace('_test', '').replace('_', '-') + ('' if g['ivSize'] == 96 else '-nonce')
        for t in g['tests']:
            # Only 12-byte nonces: any other length must be refused.
            valid = res(t) if g['ivSize'] == 96 else '0'
            add(suite, key_of(t), 'aead', alg, t['key'], t['iv'], h(t['aad']), h(t['msg']), h(t['ct']), t['tag'], '1' if valid == '1' else '0')
for g in wp('x25519_test')['testGroups']:
    for t in g['tests']:
        add('wp-x25519', key_of(t), 'x25519', t['private'], t['public'], t['shared'], '0' if t['result'] == 'invalid' else '1')
for g in wp('ed25519_test')['testGroups']:
    pk = g['publicKey']['pk']
    for t in g['tests']:
        add('wp-ed25519', key_of(t), 'ed25519', pk, h(t['msg']), h(t['sig']), res(t))
for name, cname in [('ecdsa_secp256r1_sha256_test', 'p256'), ('ecdsa_secp256r1_sha512_test', 'p256'),
                    ('ecdsa_secp384r1_sha384_test', 'p384'), ('ecdsa_secp384r1_sha256_test', 'p384'),
                    ('ecdsa_secp384r1_sha512_test', 'p384')]:
    for g in wp(name)['testGroups']:
        alg = H[g['sha']]
        pk = g['publicKey']['uncompressed']
        for t in g['tests']:
            add('wp-' + name.replace('_test', '').replace('_', '-'), key_of(t), 'ecdsa', cname, alg, ref(pk), h(t['msg']), h(t['sig']), res(t))
for name in ['hkdf_sha256_test', 'hkdf_sha384_test', 'hkdf_sha512_test']:
    alg = name.split('_')[1]
    for g in wp(name)['testGroups']:
        for t in g['tests']:
            add('wp-' + name.replace('_test', '').replace('_', '-'), key_of(t), 'hkdf', alg, h(t['ikm']), h(t['salt']), h(t['info']), str(t['size']), h(t['okm']), res(t))
for name in ['hmac_sha256_test', 'hmac_sha384_test', 'hmac_sha512_test']:
    alg = name.split('_')[1]
    for g in wp(name)['testGroups']:
        for t in g['tests']:
            add('wp-' + name.replace('_test', '').replace('_', '-'), key_of(t), 'hmac', alg, h(t['key']), h(t['msg']), h(t['tag']), res(t))
for bits in [2048, 3072, 4096]:
    for alg in ['sha256', 'sha384', 'sha512']:
        name = f'rsa_signature_{bits}_{alg}_test'
        for g in wp(name)['testGroups']:
            n = norm_hex(g['publicKey']['modulus'])
            e = norm_hex(g['publicKey']['publicExponent'])
            for t in g['tests']:
                add(f'wp-rsa-pkcs1-{bits}', key_of(t), 'rsa', 'pkcs1', alg, ref(n), e, h(t['msg']), h(t['sig']), '0', res(t))
    for g in wp(f'rsa_pkcs1_{bits}_sig_gen_test')['testGroups']:
        alg = H.get(g['sha'])
        if alg is None:
            continue
        pem = g['privateKeyPem']
        der = base64.b64decode(''.join(l for l in pem.splitlines() if not l.startswith('-----'))).hex()
        for t in g['tests']:
            if t['result'] != 'invalid':
                add(f'wp-rsa-pkcs1-sign-{bits}', key_of(t), 'rsa-sign', 'pkcs1', alg, ref(der), h(t['msg']), '-', t['sig'])
for name in ['rsa_pss_2048_sha256_mgf1_0_test', 'rsa_pss_2048_sha256_mgf1_32_test', 'rsa_pss_2048_sha384_mgf1_48_test',
             'rsa_pss_3072_sha256_mgf1_32_test', 'rsa_pss_4096_sha256_mgf1_32_test', 'rsa_pss_4096_sha384_mgf1_48_test',
             'rsa_pss_4096_sha512_mgf1_32_test', 'rsa_pss_4096_sha512_mgf1_64_test', 'rsa_pss_misc_test']:
    for g in wp(name)['testGroups']:
        alg = H.get(g['sha'])
        if alg is None or g.get('mgf') != 'MGF1' or g.get('mgfSha') != g['sha'] or g['keySize'] < 2048:
            continue
        n = norm_hex(g['publicKey']['modulus'])
        e = norm_hex(g['publicKey']['publicExponent'])
        for t in g['tests']:
            add('wp-' + name.replace('_test', '').replace('_', '-'), key_of(t), 'rsa', 'pss', alg, ref(n), e, h(t['msg']), h(t['sig']), str(g['sLen']), res(t))


def sha(x):
    return hashlib.sha256(bytes.fromhex(x)).hexdigest()


for g in wp('mlkem_768_keygen_seed_test')['testGroups']:
    for t in g['tests']:
        add('wp-mlkem768-keygen', key_of(t), 'mlkem-keygen', t['seed'][:64], t['seed'][64:], sha(t['ek']), sha(t['dk']))
for g in wp('mlkem_768_encaps_test')['testGroups']:
    for t in g['tests']:
        add('wp-mlkem768-encaps', key_of(t), 'mlkem-encaps', t['ek'], t['m'], h(t.get('K', '')), sha(t['c']) if t.get('c') else '-', res(t))
for g in wp('mlkem_768_semi_expanded_decaps_test')['testGroups']:
    for t in g['tests']:
        add('wp-mlkem768-decaps', key_of(t), 'mlkem-decaps', h(t['dk']), h(t['c']), h(t.get('K', '')), res(t))
for g in wp('mlkem_768_test')['testGroups']:
    for t in g['tests']:
        add('wp-mlkem768', key_of(t), 'mlkem-seed', t['seed'][:64], t['seed'][64:], sha(t['ek']) if t.get('ek') else '-', h(t['c']), h(t.get('K', '')), res(t))

# Output.
LIMIT = collections.defaultdict(lambda: 16)
for s in suites:
    if 'mlkem' in s:
        LIMIT[s] = 3
    if s.startswith('cavp-ecdsa'):
        LIMIT[s] = 3
    if s.startswith('cavp-sha') or s.startswith('cavp-shake') or s.startswith('cavp-hmac'):
        LIMIT[s] = 12
    if 'monte' in s:
        LIMIT[s] = 2
    if 'rsa' in s:
        LIMIT[s] = 3 if '4096' in s or '3072' in s else 5
    if s in ('wp-rsa-pss-4096-sha384-mgf1-48', 'wp-rsa-pss-4096-sha512-mgf1-32'):
        LIMIT[s] = 0
    if 'decaps' in s:
        LIMIT[s] = 2
    if 'LongMsg' in s or s.endswith('-varout'):
        LIMIT[s] = 6


def pick(cases, limit, suite):
    # Every distinct (flags, result) first, then evenly spaced others;
    # long lines (big messages / outputs) only where they are the point.
    if 'mlkem' not in suite and '4096' not in suite:
        short = [c for c in cases if len(c[1]) <= 1600]
        longs = [c for c in cases if 1600 < len(c[1]) <= 3000 and 'hkdf' not in suite]
        cases = short + longs[:1]
    chosen = []
    if limit == 0:
        return []
    seen = set()
    for i, (k, l) in enumerate(cases):
        if k is not None and k not in seen and len(chosen) < limit:
            seen.add(k)
            chosen.append(i)
    rest = [i for i in range(len(cases)) if i not in set(chosen)]
    room = max(0, limit - len(chosen))
    if rest and room:
        step = max(1, len(rest) // room)
        chosen += rest[::step][:room]
    return [cases[i][1] for i in sorted(set(chosen))]


def write(path, subset):
    lines = []
    for s, cases in suites.items():
        lines += pick(cases, LIMIT[s], s) if subset else [l for _, l in cases]
    used = set(re.findall(r'@(k\d+)', '\n'.join(lines)))
    out = ['def %s %s' % (n, v) for n, v in def_lines.items() if n in used] + lines
    open(path, 'w').write('\n'.join(out) + '\n')
    return len(lines)


if full_out:
    nf = write(full_out, False)
    print(f'full: {nf} cases, sha256 {hashlib.sha256(open(full_out, "rb").read()).hexdigest()}')
print(f'subset: {write(sub_out, True)} cases, {os.path.getsize(sub_out)} bytes')
