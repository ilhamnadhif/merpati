"""Build tests/fixtures/edgegrid/extra_cases.json.

Usage: edgegrid_extra.py TESTDATA.json NODE_TEST_DATA.json PHP_TESTDATA.json OUT.json

- copies cases from edgegrid-node test.js / edgegrid-php testdata.json that the
  python testcases.json lacks (signatures as published upstream);
- signs further inputs with edgegrid-python (EdgeGridAuthHeaders.make_auth_header).
"""

import json
import sys
from urllib.parse import urljoin

import requests
from akamai.edgegrid import edgegrid as eg

testdata = json.load(open(sys.argv[1]))
php = json.load(open(sys.argv[3]))
BASE = testdata["base_url"]

cases = []

# edgegrid-node test/test.js (signatures copied from the test file; node never
# signs headers and always hashes up to 131072 bytes).
NODE_SIG = {
    "t4": "YgMcMzBrimnBmp7wxzjirUsAcC0UK6MVPydEpjKVcHc=",
    "json": "lUcTuRl/Iy5vmBp7uNvya9BoRaA9/oyHKC+pCDOlg1s=",
}
PREFIX = (
    "EG1-HMAC-SHA256 client_token=akab-client-token-xxx-xxxxxxxxxxxxxxxx;"
    "access_token=akab-access-token-xxx-xxxxxxxxxxxxxxxx;timestamp=20140321T19:34:21+0000;"
    "nonce=nonce-xx-xxxx-xxxx-xxxx-xxxxxxxxxxxx;signature="
)
# node's "POST too large" (Qevb0l2a...) is not used: node passes the file as a
# Buffer, which its contentHash serializes like an object ("0=0&1=0&..."),
# so the hash is not over the body bytes. The same input signed by
# edgegrid-python is below.
cases.append({
    "testName": "node: simple header signing with GET (no headers to sign)",
    "request": {"method": "GET", "path": "/testapi/v1/t4", "headers": [{"X-Test1": "test-simple-header"}]},
    "expectedAuthorization": PREFIX + NODE_SIG["t4"],
})
cases.append({
    "testName": "node: POST request when max_body is provided in .edgerc file",
    "request": {
        "method": "POST",
        "path": "/testapi/v1/t3",
        "data": "{\"name\":\"text24.devexp-cli-dns-test.net\",\"type\":\"SRV\",\"ttl\":300,"
        "\"zone\":\"devexp-cli-dns-test.net\",\"rdata\":[\"10 40 5061 small.example.com\","
        "\"20 10 5060 tiny.example.com\"]}",
    },
    "expectedAuthorization": PREFIX + NODE_SIG["json"],
})

# edgegrid-php tests/testdata.json cases missing from python's testcases.json.
for t in php["tests"]:
    if t["testName"] in ("POST nil body", "GET with query args as option"):
        req = dict(t["request"])
        if req.get("data") is None:
            req.pop("data", None)
        cases.append({
            "testName": "php: " + t["testName"],
            "auth": {"headersToSign": php["headers_to_sign"], "maxBodySize": php["max_body"]},
            "request": req,
            "expectedAuthorization": t["expectedAuthorization"],
        })


def py_case(name, method, path, headers=None, data=None, headers_to_sign=(), max_body=131072, base=BASE):
    signer = eg.EdgeGridAuthHeaders(
        testdata["client_token"], testdata["client_secret"], testdata["access_token"],
        headers_to_sign=list(headers_to_sign), max_body=max_body,
    )
    hdrs = {}
    for h in headers or []:
        hdrs.update(h)
    req = requests.Request(method, urljoin(base, path), headers=hdrs, data=data).prepare()
    auth = signer.make_auth_header(req, testdata["timestamp"], testdata["nonce"])
    r = {"method": method, "path": path}
    if headers:
        r["headers"] = headers
    if isinstance(data, bytes) and data == bytes(len(data)):
        r["zero_body_len"] = len(data)
    elif isinstance(data, bytes):
        r["data_hex"] = data.hex()
    elif data is not None:
        r["data"] = data
    case = {"testName": "python 2.0.8: " + name, "request": r, "expectedAuthorization": auth}
    a = {}
    if headers_to_sign:
        a["headersToSign"] = ", ".join(headers_to_sign)
    if max_body != 131072:
        a["maxBodySize"] = str(max_body)
    if a:
        case["auth"] = a
    if base != BASE:
        case["base_url"] = base
    return case


cases += [
    py_case("non-default port is part of the host", "GET", "/x?y=1", base="https://h.luna.akamaiapis.net:8443/"),
    py_case(
        "explicit Host header is signed instead of the URL host",
        "GET", "/x", headers=[{"Host": "api.example.com"}], base="https://203.0.113.7/",
    ),
    py_case("binary POST body", "POST", "/bin", data=bytes(range(256)) * 3, headers=[{"Content-Type": "application/octet-stream"}]),
    py_case("200000-byte body (node's test_body_data.txt) hashed up to the default 131072", "POST", "/testapi/v1/t3", data=bytes(200000)),
    py_case("POST body cut to max_body=16", "POST", "/cut", data="0123456789abcdefTRUNCATED", max_body=16),
    py_case(
        "header values: tabs and runs of spaces collapse, names case-insensitive, configured order",
        "GET", "/h",
        headers=[{"x-b": "two\t\twords"}, {"X-A": "a  b   c"}, {"X-Unsigned": "nope"}],
        headers_to_sign=["X-A", "x-B", "X-Missing"],
    ),
    py_case("percent-encoded path and query", "GET", "/caf%C3%A9/a%20b?q=%C3%A9t%C3%A9&x=a+b&empty="),
    py_case("DELETE with body is not hashed", "DELETE", "/d", data="body"),
]

json.dump({
    "_source": "See README.md: edgegrid-node test/test.js, edgegrid-php tests/testdata.json (Apache-2.0), "
    "and edgegrid-python 2.0.8 run on the inputs below.",
    "cases": cases,
}, open(sys.argv[4], "w"), indent=1)
print(len(cases), "cases")
