"""ASAP cross-checks against atlassian-jwt-auth (the reference library).

Usage:
  asap_vectors.py gen FIXTURES_DIR OUT.json   -> tokens signed by atlassian-jwt-auth with fixed time/jti
  asap_vectors.py verify FIXTURES_DIR IN.json -> verify Merpati tokens with JWTAuthVerifier

FIXTURES_DIR holds the PEM keys (crates/merpati-auth/tests/fixtures).
"""

import datetime
import json
import os
import sys

import atlassian_jwt_auth
import atlassian_jwt_auth.signer as S
from atlassian_jwt_auth.key import BasePublicKeyRetriever
from atlassian_jwt_auth.verifier import JWTAuthVerifier

NOW = 1761200000  # 2025-10-23T06:13:20Z
KEYS = {
    "rsa": ("rsa_pkcs8.pem", "rsa_pub.pem"),
    "ec256": ("ec_p256.pem", "ec_p256_pub.pem"),
    "ec384": ("ec_p384.pem", "ec_p384_pub.pem"),
}
ALG_KEY = {
    "RS256": "rsa", "RS384": "rsa", "RS512": "rsa",
    "PS256": "rsa", "PS384": "rsa", "PS512": "rsa",
    "ES256": "ec256", "ES384": "ec384",
}


def read(d, name):
    return open(os.path.join(d, name)).read()


class _FixedRandom:
    def getrandbits(self, n):
        return 3141592653


S.random.SystemRandom = _FixedRandom


def cases():
    base = {"iss": "merpati-test", "kid": "merpati-test/key1", "aud": "svc-b", "lifetime": 600, "claims": {}}
    out = [dict(base, name=f"{alg} single audience", alg=alg) for alg in ALG_KEY]
    out.append(dict(base, name="RS256 several audiences", alg="RS256", aud=["svc-b", "svc-c"]))
    out.append(dict(base, name="RS256 additional claims", alg="RS256", claims={"scope": "read", "ctx": {"a": 1}}))
    out.append(dict(base, name="RS256 kid equal to issuer", alg="RS256", kid="merpati-test"))
    out.append(dict(base, name="ES256 nested key path, max lifetime", alg="ES256", kid="merpati-test/2025/k-1.pem", lifetime=3600))
    return out


def gen(fixtures, out_path):
    result = []
    for c in cases():
        priv, _ = KEYS[ALG_KEY[c["alg"]]]
        signer = atlassian_jwt_auth.create_signer(
            c["iss"], c["kid"], read(fixtures, priv), algorithm=c["alg"],
            lifetime=datetime.timedelta(seconds=c["lifetime"]),
        )
        signer._now = lambda: datetime.datetime.fromtimestamp(NOW, datetime.timezone.utc)
        token = signer.generate_jwt(c["aud"], additional_claims=c["claims"]).decode()
        verify_one(fixtures, c, token)
        result.append(dict(c, now=NOW, private_key=priv, public_key=KEYS[ALG_KEY[c["alg"]]][1], token=token))
    json.dump({"_source": "atlassian-jwt-auth 22.0.1 (MIT); see README.md", "cases": result},
              open(out_path, "w"), indent=1)
    print(len(result), "reference tokens")


class _Static(BasePublicKeyRetriever):
    def __init__(self, pem):
        self.pem = pem

    def retrieve(self, key_identifier, **kwargs):
        return self.pem


def verify_one(fixtures, c, token):
    _, pub = KEYS[ALG_KEY[c["alg"]]]
    verifier = JWTAuthVerifier(_Static(read(fixtures, pub)))
    aud = c["aud"] if isinstance(c["aud"], str) else c["aud"][-1]
    # Fixed issue time in the past: widen the clock leeway so only the
    # signature, aud, kid ownership, sub, lifetime and jti rules are checked.
    claims = verifier.verify_jwt(token, aud, leeway=10 * 365 * 86400)
    assert claims["iss"] == c["iss"]
    return claims


def verify(fixtures, in_path):
    doc = json.load(open(in_path))
    for c in doc["merpati"]:
        claims = verify_one(fixtures, c, c["token"])
        print("verified by atlassian-jwt-auth:", c["name"], sorted(claims))


if __name__ == "__main__":
    if sys.argv[1] == "gen":
        gen(sys.argv[2], sys.argv[3])
    else:
        verify(sys.argv[2], sys.argv[3])
