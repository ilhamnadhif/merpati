# NTLM test vectors

`pyspnego_vectors.json` holds AUTHENTICATE messages used by the unit tests in
`src/ntlm.rs`. They were produced by `generate_vectors.py` (kept here for
regeneration; run it with `python -I` in a venv with `pyspnego==0.12.4`) using
pyspnego's pure-Python NTLM implementation
(<https://github.com/jborean93/pyspnego>, MIT license).

The script fixes pyspnego's randomness (client challenge, random session key,
client time) by patching `os.urandom` and `FileTime.now`, and feeds it
Merpati's own NEGOTIATE message so the MIC covers the same bytes Merpati sends.
Besides that it patches only:

- `md5` of a marker channel-bindings object, to get the all-zero
  MsvAvChannelBindings Merpati sends when no TLS channel binding is known;
- `Version.get_current` / `_get_workstation`, to Merpati's values.

`vectors`: byte-for-byte pyspnego output; each one was also accepted by
pyspnego's acceptor (NT proof, MIC, channel bindings, exported session key).

`accepted`: cases where Merpati deliberately differs from pyspnego's initiator.
The expected bytes are Merpati's, and the generator's `accept` mode checked them
with pyspnego's acceptor:

- KEY_EXCH without SIGN/SEAL: pyspnego sends a 1-byte EncryptedRandomSessionKey;
  Merpati sends RC4K(KeyExchangeKey, KeyExchangeKey), 16 bytes, which decrypts
  to the same exported session key for servers that always decrypt (Samba) and
  is ignored by servers following MS-NLMP 3.2.5.1.2.
- Server without NEGOTIATE_VERSION: Merpati keeps the 8-byte Version field
  (zeroed) so the MIC stays at offset 72 as Windows expects; pyspnego drops it
  and moves the MIC to offset 64.

Credentials and keys in the vectors are made up for testing.
