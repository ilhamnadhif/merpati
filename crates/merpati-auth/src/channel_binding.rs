//! TLS channel bindings of type `tls-server-end-point` (RFC 5929 section 4),
//! which NTLM sends as MsvAvChannelBindings for Extended Protection.
//!
//! The binding is `tls-server-end-point:` followed by a hash of the server's
//! leaf certificate (DER). The hash is the one of the certificate's
//! signature algorithm (for RSASSA-PSS the one in its parameters), except
//! that MD5 and SHA-1 become SHA-256. Signature algorithms without a single
//! hash function of their own (Ed25519, Ed448) use SHA-256 too, like
//! requests-ntlm and pypsrp; so do ones we do not recognize (SHA-3 based,
//! GOST, SM2), for which those send no binding at all. requests-ntlm 1.3
//! picks the same hash for every certificate in `tests/fixtures/tls_binding`.
//!
//! Only the certificate's `signatureAlgorithm` is needed, so a minimal DER
//! walk replaces an X.509 parser. Malformed certificates also fall back to
//! SHA-256 (rustls has already parsed and checked the certificate).

use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};

use crate::util::der_tlv as tlv;

/// Hash functions used for the certificate hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CertHash {
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

const SEQUENCE: u8 = 0x30;
const OID: u8 = 0x06;
/// `[0]` EXPLICIT, the `hashAlgorithm` of RSASSA-PSS-params.
const CONTEXT_0: u8 = 0xa0;

/// 1.2.840.113549.1.1 (PKCS #1) prefix; the last arc names the algorithm.
const PKCS1: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01];
/// 1.2.840.10045.4 (ANSI X9.62 ECDSA signatures).
const ECDSA: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04];
/// 2.16.840.1.101.3.4.3 (NIST DSA/ECDSA/RSA signature algorithms).
const NIST_SIG: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x03];
/// 2.16.840.1.101.3.4.2 (NIST hash algorithms).
const NIST_HASH: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02];

/// The application data of the `tls-server-end-point` channel binding for a
/// server whose leaf certificate is `cert_der`.
pub(crate) fn tls_server_end_point(cert_der: &[u8]) -> Vec<u8> {
    let mut data = b"tls-server-end-point:".to_vec();
    match certificate_hash(cert_der) {
        CertHash::Sha224 => data.extend_from_slice(&Sha224::digest(cert_der)),
        CertHash::Sha256 => data.extend_from_slice(&Sha256::digest(cert_der)),
        CertHash::Sha384 => data.extend_from_slice(&Sha384::digest(cert_der)),
        CertHash::Sha512 => data.extend_from_slice(&Sha512::digest(cert_der)),
    }
    data
}

/// The hash function RFC 5929 picks for `cert_der` (see the module docs).
pub(crate) fn certificate_hash(cert_der: &[u8]) -> CertHash {
    signature_hash(cert_der).unwrap_or(CertHash::Sha256)
}

/// The single hash function of the certificate's signature algorithm, with
/// MD5 and SHA-1 already mapped to SHA-256; `None` when there is none or it
/// is not recognized.
fn signature_hash(cert_der: &[u8]) -> Option<CertHash> {
    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signatureValue }
    let cert = expect(cert_der, SEQUENCE)?;
    let (_, _tbs, rest) = tlv(cert)?;
    // AlgorithmIdentifier ::= SEQUENCE { algorithm OID, parameters ANY OPTIONAL }
    let alg = expect(rest, SEQUENCE)?;
    let (tag, oid, params) = tlv(alg)?;
    if tag != OID {
        return None;
    }
    if let Some(arc) = oid.strip_prefix(PKCS1) {
        return match arc {
            // md2/md4/md5/sha1WithRSAEncryption.
            [0x02..=0x05] => Some(CertHash::Sha256),
            [0x0b] => Some(CertHash::Sha256),
            [0x0c] => Some(CertHash::Sha384),
            [0x0d] => Some(CertHash::Sha512),
            [0x0e] => Some(CertHash::Sha224),
            // id-RSASSA-PSS: the hash is in the parameters.
            [0x0a] => pss_hash(params),
            _ => None,
        };
    }
    if let Some(arc) = oid.strip_prefix(ECDSA) {
        return match arc {
            // ecdsa-with-SHA1.
            [0x01] => Some(CertHash::Sha256),
            // ecdsa-with-SHA224/256/384/512.
            [0x03, 0x01] => Some(CertHash::Sha224),
            [0x03, 0x02] => Some(CertHash::Sha256),
            [0x03, 0x03] => Some(CertHash::Sha384),
            [0x03, 0x04] => Some(CertHash::Sha512),
            _ => None,
        };
    }
    // id-dsa-with-sha1 (1.2.840.10040.4.3).
    if oid == [0x2a, 0x86, 0x48, 0xce, 0x38, 0x04, 0x03] {
        return Some(CertHash::Sha256);
    }
    match oid.strip_prefix(NIST_SIG)? {
        // id-dsa-with-sha224/256/384/512.
        [0x01] => Some(CertHash::Sha224),
        [0x02] => Some(CertHash::Sha256),
        [0x03] => Some(CertHash::Sha384),
        [0x04] => Some(CertHash::Sha512),
        _ => None,
    }
}

/// `hashAlgorithm` of RSASSA-PSS-params (RFC 4055); absent means SHA-1.
fn pss_hash(params: &[u8]) -> Option<CertHash> {
    let mut fields = expect(params, SEQUENCE)?;
    while let Some((tag, contents, rest)) = tlv(fields) {
        if tag == CONTEXT_0 {
            let alg = expect(contents, SEQUENCE)?;
            let (tag, oid, _) = tlv(alg)?;
            if tag != OID {
                return None;
            }
            // id-sha1 (1.3.14.3.2.26).
            if oid == [0x2b, 0x0e, 0x03, 0x02, 0x1a] {
                return Some(CertHash::Sha256);
            }
            return match oid.strip_prefix(NIST_HASH)? {
                [0x01] => Some(CertHash::Sha256),
                [0x02] => Some(CertHash::Sha384),
                [0x03] => Some(CertHash::Sha512),
                [0x04] => Some(CertHash::Sha224),
                _ => None,
            };
        }
        fields = rest;
    }
    Some(CertHash::Sha256)
}

/// Contents of the DER element at the start of `b` if its tag is `tag`.
fn expect(b: &[u8], tag: u8) -> Option<&[u8]> {
    tlv(b).filter(|(t, _, _)| *t == tag).map(|(_, contents, _)| contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!("{}/tests/fixtures/tls_binding/{name}.pem", env!("CARGO_MANIFEST_DIR"));
        CertificateDer::from_pem_file(&path).unwrap_or_else(|e| panic!("{path}: {e}")).to_vec()
    }

    fn rcgen_cert(alg: &'static rcgen::SignatureAlgorithm) -> Vec<u8> {
        let key = rcgen::KeyPair::generate_for(alg).unwrap();
        let params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        params.self_signed(&key).unwrap().der().to_vec()
    }

    fn binding(hash: &[u8]) -> Vec<u8> {
        [b"tls-server-end-point:".as_slice(), hash].concat()
    }

    #[test]
    fn ecdsa_sha256_and_sha384_certificates() {
        let p256 = rcgen_cert(&rcgen::PKCS_ECDSA_P256_SHA256);
        assert_eq!(certificate_hash(&p256), CertHash::Sha256);
        assert_eq!(tls_server_end_point(&p256), binding(&Sha256::digest(&p256)));

        let p384 = rcgen_cert(&rcgen::PKCS_ECDSA_P384_SHA384);
        assert_eq!(certificate_hash(&p384), CertHash::Sha384);
        let data = tls_server_end_point(&p384);
        assert_eq!(data, binding(&Sha384::digest(&p384)));
        assert_eq!(data.len(), 21 + 48);
    }

    #[test]
    fn eddsa_certificates_use_sha256() {
        let ed = rcgen_cert(&rcgen::PKCS_ED25519);
        assert_eq!(signature_hash(&ed), None);
        assert_eq!(tls_server_end_point(&ed), binding(&Sha256::digest(&ed)));
        assert_eq!(certificate_hash(&fixture("ed448")), CertHash::Sha256);
    }

    /// Certificates made by `tests/fixtures/tls_binding/generate.sh`.
    #[test]
    fn openssl_certificates() {
        for (name, hash) in [
            // MD5 and SHA-1 are upgraded to SHA-256.
            ("rsa_md5", CertHash::Sha256),
            ("rsa_sha1", CertHash::Sha256),
            ("ecdsa_sha1", CertHash::Sha256),
            ("rsa_sha224", CertHash::Sha224),
            ("rsa_sha512", CertHash::Sha512),
            // RSASSA-PSS: the hash comes from the parameters.
            ("rsa_pss_sha384", CertHash::Sha384),
        ] {
            let der = fixture(name);
            assert_eq!(signature_hash(&der), Some(hash), "{name}");
        }
        let der = fixture("rsa_sha512");
        assert_eq!(tls_server_end_point(&der), binding(&Sha512::digest(&der)));
    }

    #[test]
    fn pss_parameters() {
        // RSASSA-PSS-params with every field defaulted: SHA-1, hence SHA-256.
        assert_eq!(pss_hash(&[SEQUENCE, 0]), Some(CertHash::Sha256));
        // hashAlgorithm [0] { sha512 }.
        let sha512 = [&[CONTEXT_0, 13, SEQUENCE, 11, OID, 9][..], NIST_HASH, &[0x03]].concat();
        let params = [&[SEQUENCE, sha512.len() as u8][..], &sha512].concat();
        assert_eq!(pss_hash(&params), Some(CertHash::Sha512));
        assert_eq!(pss_hash(&[]), None);
    }

    #[test]
    fn malformed_certificates_fall_back_to_sha256() {
        let der = rcgen_cert(&rcgen::PKCS_ECDSA_P384_SHA384);
        for bad in [&b""[..], b"\x30", b"\x30\x84\xff\xff\xff\xff", &der[..der.len() / 2], b"\x04\x00"] {
            assert_eq!(certificate_hash(bad), CertHash::Sha256, "{bad:02x?}");
        }
        // Long-form lengths parse; indefinite ones are not DER.
        assert_eq!(tlv(&[0x04, 0x81, 0x01, 0xaa, 0xbb]), Some((0x04, &[0xaa][..], &[0xbb][..])));
        assert_eq!(tlv(&[0x04, 0x80]), None);
        assert_eq!(tlv(&[0x04, 0x02, 0xaa]), None);
    }
}
