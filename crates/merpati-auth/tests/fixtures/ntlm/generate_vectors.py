"""Generate NTLM AUTHENTICATE test vectors with pyspnego's pure-Python NTLM.

Usage:
  ntlm_vectors.py gen OUT.json            -> byte-for-byte vectors + cases for our acceptor check
  ntlm_vectors.py accept IN.json OUT.json -> validate Merpati's messages with pyspnego's acceptor

Randomness (client challenge, exported session key, client time) is fixed by
patching pyspnego's module-level `os.urandom` / `FileTime.now`. The NEGOTIATE
covered by the MIC is Merpati's own (passed in), as on the wire.
"""

import hashlib
import json
import os
import sys
import tempfile

import spnego
import spnego._ntlm as N
from spnego._ntlm_raw import messages as M
from spnego._ntlm_raw.crypto import rc4k
from spnego.channel_bindings import GssChannelBindings

# Merpati's NEGOTIATE (ntlm::negotiate_message), 0xa0088207 flags, empty buffers.
MERPATI_NEGOTIATE = bytes.fromhex("4e544c4d53535000" "01000000" "078208a0" "0000000020000000" "0000000020000000")
VERSION = M.Version(10, 0, 19041)  # what Merpati sends when VERSION is negotiated

F = M.NegotiateFlags
WINDOWS_FLAGS = (
    F.unicode | F.request_target | F.ntlm | F.always_sign | F.target_type_domain
    | F.extended_session_security | F.target_info | F.version | F.key_128 | F.key_56
)

_real_md5 = N.md5
ZERO_CB_MARKER = GssChannelBindings(application_data=b"merpati:no-channel-binding")


def _md5(data):
    # Merpati sends an all-zero MsvAvChannelBindings when no TLS binding is known.
    if data == ZERO_CB_MARKER.pack():
        return b"\x00" * 16
    return _real_md5(data)


class _FixedOs:
    def __init__(self):
        self.values = {}

    def __getattr__(self, name):
        return getattr(os, name)

    def urandom(self, n):
        return self.values[n]


FIXED_OS = _FixedOs()
N.os = FIXED_OS
N.md5 = _md5
N.Version.get_current = staticmethod(lambda: VERSION)
_NOW = {"value": None}
M.FileTime.now = classmethod(lambda cls, tz=None: M.FileTime.unpack(_NOW["value"]))
N.FileTime.now = M.FileTime.now


def challenge(flags, server_challenge, target_info_pairs, version=True):
    ti = M.TargetInfo()
    for k, v in target_info_pairs:
        ti[k] = v
    kwargs = {"version": M.Version(10, 0, 20348)} if version and flags & F.version else {}
    return M.Challenge(flags, server_challenge, target_name="CORP", target_info=ti, **kwargs).pack()


def server_pairs(timestamp=True, extra=()):
    pairs = [
        (M.AvId.nb_domain_name, "CORP"),
        (M.AvId.nb_computer_name, "WEB01"),
        (M.AvId.dns_domain_name, "corp.example"),
        (M.AvId.dns_computer_name, "web01.corp.example"),
        (M.AvId.dns_tree_name, "corp.example"),
    ]
    if timestamp:
        pairs.append((M.AvId.timestamp, M.FileTime.unpack(bytes.fromhex("80d8a1c4f2a6db01"))))
    pairs.extend(extra)
    return pairs


def initiate(case, substitute_key_exch=False):
    FIXED_OS.values = {8: bytes.fromhex(case["client_challenge"]), 16: bytes.fromhex(case["session_key"])}
    _NOW["value"] = bytes.fromhex(case["now"])
    N._get_workstation = lambda: case["workstation"]
    cb = case["channel_bindings"]
    bindings = GssChannelBindings(application_data=bytes.fromhex(cb)) if cb is not None else ZERO_CB_MARKER
    user = f"{case['domain']}\\{case['user']}" if case["domain"] else case["user"]
    host = case["target_name"].split("/", 1)[1]
    ctx = N.NTLMProxy(user, case["password"], hostname=host, service="HTTP", channel_bindings=bindings)
    ctx.step()
    ctx._temp_negotiate = M.Negotiate.unpack(bytes.fromhex(case["negotiate"]))

    real_auth = N.Authenticate

    def auth(flags, lm, nt, **kw):
        # Merpati's one deliberate difference: KEY_EXCH without SIGN/SEAL
        # sends RC4K(KeyExchangeKey, KeyExchangeKey) instead of one zero byte.
        if substitute_key_exch and kw.get("encrypted_session_key") == b"\x00":
            kw["encrypted_session_key"] = rc4k(ctx._session_key, ctx._session_key)
        return real_auth(flags, lm, nt, **kw)

    N.Authenticate = auth
    try:
        out = ctx.step(bytes.fromhex(case["challenge"]))
    finally:
        N.Authenticate = real_auth
    return out, ctx.session_key


def accept(case, authenticate):
    """Verify NT proof, MIC and channel bindings with pyspnego's acceptor."""
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
        f.write(f"{case['domain']}:{case['user']}:{case['password']}\n")
        path = f.name
    os.environ["NTLM_USER_FILE"] = path
    try:
        cb = case["channel_bindings"]
        bindings = GssChannelBindings(application_data=bytes.fromhex(cb)) if cb is not None else None
        acc = N.NTLMProxy(usage="accept")
        acc._temp_negotiate = M.Negotiate.unpack(bytes.fromhex(case["negotiate"]))
        acc._temp_challenge = M.Challenge.unpack(bytes.fromhex(case["challenge"]))
        acc._step_accept_authenticate(authenticate, channel_bindings=bindings)
        assert acc.complete
        auth = M.Authenticate.unpack(authenticate)
        has_mic = auth.mic is not None and auth.mic != b"\x00" * 16
        return acc.session_key, has_mic
    finally:
        os.unlink(path)
        del os.environ["NTLM_USER_FILE"]


def base_case(name, flags, pairs, **over):
    case = {
        "name": name,
        "user": "alice",
        "password": "S3cret!pw",
        "domain": "CORP",
        "workstation": "MERPATI",
        "target_name": "HTTP/web01.corp.example",
        "channel_bindings": None,
        "client_challenge": "0102030405060708",
        "now": "00e057d5f2a6db01",
        "session_key": "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf",
        "negotiate": MERPATI_NEGOTIATE.hex(),
        "server_challenge": "8877665544332211",
    }
    case.update(over)
    case["challenge"] = challenge(flags, bytes.fromhex(case["server_challenge"]), pairs).hex()
    return case


def gen(out_path):
    tls = b"tls-server-end-point:" + hashlib.sha256(b"merpati test certificate").digest()
    vectors = [
        base_case("windows server, timestamp (MIC), no key exchange", WINDOWS_FLAGS, server_pairs()),
        base_case(
            "key exchange with signing: random exported session key",
            WINDOWS_FLAGS | F.key_exch | F.sign,
            server_pairs(),
        ),
        base_case(
            "key exchange with sealing",
            WINDOWS_FLAGS | F.key_exch | F.seal | F.sign,
            server_pairs(),
            session_key="0f1e2d3c4b5a69788796a5b4c3d2e1f0",
        ),
        base_case(
            "TLS channel binding (tls-server-end-point)",
            WINDOWS_FLAGS,
            server_pairs(),
            channel_bindings=tls.hex(),
            target_name="HTTP/web01.corp.example:8443",
        ),
        base_case("no server timestamp: LMv2, no MIC, client time", WINDOWS_FLAGS, server_pairs(timestamp=False)),
        base_case(
            "server MsvAvFlags kept and MIC bit added",
            WINDOWS_FLAGS,
            server_pairs(extra=[(M.AvId.flags, M.AvFlags(1))]),
        ),
        base_case(
            "non-ASCII credentials",
            WINDOWS_FLAGS,
            server_pairs(),
            user="Jürgen",
            password="pässwörd€",
            domain="MÜNCHEN",
            workstation="LAPTOP-Å",
        ),
        base_case("empty domain", WINDOWS_FLAGS, server_pairs(), domain=""),
    ]
    for case in vectors:
        msg, key = initiate(case)
        case["authenticate"] = msg.hex()
        case["exported_session_key"] = key.hex()
        acc_key, has_mic = accept(case, msg)
        assert acc_key == key, case["name"]
        assert has_mic == ("no server timestamp" not in case["name"]), case["name"]
        del case["server_challenge"]

    # Cases where Merpati deliberately differs from pyspnego's initiator; the
    # expected bytes are checked with pyspnego's acceptor in `accept` mode.
    accepted = [
        base_case(
            "key exchange without signing or sealing",
            WINDOWS_FLAGS | F.key_exch,
            server_pairs(),
        ),
        base_case(
            "server without NEGOTIATE_VERSION: zero Version field before the MIC",
            WINDOWS_FLAGS & ~F.version,
            server_pairs(),
        ),
    ]
    for case in accepted:
        # pyspnego's own output for reference (not what Merpati sends).
        msg, key = initiate(case, substitute_key_exch=True)
        case["pyspnego_authenticate"] = msg.hex()
        del case["server_challenge"]
    json.dump({"vectors": vectors, "accepted": accepted}, open(out_path, "w"), indent=1, ensure_ascii=False)


def check(in_path, out_path):
    doc = json.load(open(in_path))
    for case in doc["accepted"]:
        msg = bytes.fromhex(case["authenticate"])
        key, has_mic = accept(case, msg)
        assert has_mic, case["name"]
        case["exported_session_key"] = key.hex()
        print("accepted:", case["name"], "MIC verified, session key", key.hex())
        if case["pyspnego_authenticate"] == case["authenticate"]:
            print("  (identical to pyspnego)")
        del case["pyspnego_authenticate"]
    json.dump(doc, open(out_path, "w"), indent=1, ensure_ascii=False)


if __name__ == "__main__":
    print("pyspnego", spnego.__version__ if hasattr(spnego, "__version__") else "?", file=sys.stderr)
    if sys.argv[1] == "gen":
        gen(sys.argv[2])
    else:
        check(sys.argv[2], sys.argv[3])
