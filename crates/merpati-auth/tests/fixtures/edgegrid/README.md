# Akamai EdgeGrid test data

Used by the unit tests in `src/edgegrid.rs`.

- `testdata.json`, `testcases.json`, `sample_file.txt`: verbatim from
  <https://github.com/akamai/AkamaiOPEN-edgegrid-python>
  (`akamai/edgegrid/test/`, master as of 2026-10-23), Apache License 2.0
  (`LICENSE-APACHE-2.0.txt`, identical in AkamaiOPEN-edgegrid-node). 12 of its
  15 cases appear with identical signatures in edgegrid-perl `t/testdata.json`,
  edgegrid-ruby `test/testdata.json` and edgegrid-php `tests/testdata.json`
  (python adds PATCH and the two `configs/111` query cases).
- `extra_cases.json`, built by `generate_extra_cases.py` (run with `python -I`
  in a venv with `edgegrid-python==2.0.8`):
  - cases from AkamaiOPEN-edgegrid-node `test/test.js` (unsigned headers,
    JSON body) and edgegrid-php
    `tests/testdata.json` (`POST nil body`, `GET with query args as option`)
    that python's file lacks, with their published signatures;
  - signatures computed by edgegrid-python 2.0.8 for inputs of our own:
    non-default port, explicit `Host` header, binary body, small max body,
    whitespace in header values, header name case and order, percent-encoded
    path and query, DELETE with a body, and node's 200000-byte
    `test_body_data.txt` (zeros). Node's own signature for that file
    (`Qevb0l2a…`) is not used: node passes it as a `Buffer`, which its
    `contentHash` serializes like an object (`0=0&1=0&…`) before hashing.

Credentials in the data are Akamai's placeholders.
