//! Challenge/response authentication with a device's existing certificate.
//!
//! The relay has no accounts. A device's address is the SHA-256 of the same
//! certificate it already pins during LAN pairing, so a paired peer can
//! compute the address without any extra exchange, and nobody can claim it
//! without the private key.

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use openssl::hash::{hash, MessageDigest};
use openssl::pkey::Id;
use openssl::sign::Verifier;
use openssl::x509::X509;

use crate::protocol::auth_sign_input;

/// A fresh challenge nonce: base64url of 32 random bytes.
pub fn new_nonce() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    openssl::rand::rand_bytes(&mut bytes).map_err(|error| error.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Device address: base64url (no padding) of SHA-256 over the certificate DER.
pub fn fingerprint(cert_der: &[u8]) -> Result<String, String> {
    let digest = hash(MessageDigest::sha256(), cert_der).map_err(|error| error.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(digest))
}

/// Base64 from Android's `Base64.DEFAULT` wraps lines every 76 characters.
/// Tolerating whitespace costs nothing and removes an interop trap.
fn decode_b64(text: &str) -> Option<Vec<u8>> {
    let compact: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    STANDARD.decode(compact).ok()
}

/// Checks that `sig_b64` is the certificate key's SHA-256 signature over the
/// challenge, and returns the device fingerprint.
///
/// Only RSA (PKCS#1 v1.5) and EC (ECDSA, DER signature) keys are accepted:
/// those are the two key types DeskLink devices generate, and refusing the
/// rest keeps unusual OpenSSL code paths off an unauthenticated input.
/// Certificate validity dates are deliberately ignored: device certificates
/// are self-signed and only prove possession of the key.
pub fn verify(cert_b64: &str, sig_b64: &str, nonce: &str, public_host: &str) -> Option<String> {
    let der = decode_b64(cert_b64)?;
    let signature = decode_b64(sig_b64)?;
    let cert = X509::from_der(&der).ok()?;
    // OpenSSL ignores bytes after the certificate. Without this check one key
    // could mint unlimited fingerprints by appending junk, each with its own
    // limits and quota.
    if cert.to_der().ok()? != der {
        return None;
    }
    let key = cert.public_key().ok()?;
    if !matches!(key.id(), Id::RSA | Id::EC) {
        return None;
    }
    let mut verifier = Verifier::new(MessageDigest::sha256(), &key).ok()?;
    verifier.update(&auth_sign_input(nonce, public_host)).ok()?;
    if !verifier.verify(&signature).ok()? {
        return None;
    }
    fingerprint(&der).ok()
}

/// Key and certificate helpers shared by the unit and end-to-end tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::ec::{EcGroup, EcKey};
    use openssl::nid::Nid;
    use openssl::pkey::{PKey, Private};
    use openssl::rsa::Rsa;
    use openssl::sign::Signer;
    use openssl::x509::{X509Builder, X509NameBuilder};

    pub(crate) struct Identity {
        pub key: PKey<Private>,
        pub cert_der: Vec<u8>,
    }

    impl Identity {
        pub fn ec() -> Self {
            let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
            Self::self_signed(PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap())
        }

        pub fn rsa() -> Self {
            Self::self_signed(PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap())
        }

        fn self_signed(key: PKey<Private>) -> Self {
            let mut name = X509NameBuilder::new().unwrap();
            name.append_entry_by_text("CN", "desklink-test").unwrap();
            let name = name.build();
            let mut builder = X509Builder::new().unwrap();
            builder.set_version(2).unwrap();
            builder.set_subject_name(&name).unwrap();
            builder.set_issuer_name(&name).unwrap();
            builder.set_pubkey(&key).unwrap();
            builder
                .set_not_before(&Asn1Time::days_from_now(0).unwrap())
                .unwrap();
            builder
                .set_not_after(&Asn1Time::days_from_now(1).unwrap())
                .unwrap();
            builder.sign(&key, MessageDigest::sha256()).unwrap();
            let cert_der = builder.build().to_der().unwrap();
            Self { key, cert_der }
        }

        pub fn cert_b64(&self) -> String {
            STANDARD.encode(&self.cert_der)
        }

        pub fn fp(&self) -> String {
            fingerprint(&self.cert_der).unwrap()
        }

        pub fn sign(&self, nonce: &str, host: &str) -> String {
            let mut signer = Signer::new(MessageDigest::sha256(), &self.key).unwrap();
            signer.update(&auth_sign_input(nonce, host)).unwrap();
            STANDARD.encode(signer.sign_to_vec().unwrap())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::Identity;
    use super::*;

    const HOST: &str = "relay.example.com";

    #[test]
    fn ec_and_rsa_signatures_are_accepted() {
        for identity in [Identity::ec(), Identity::rsa()] {
            let nonce = new_nonce().unwrap();
            let sig = identity.sign(&nonce, HOST);
            assert_eq!(
                verify(&identity.cert_b64(), &sig, &nonce, HOST),
                Some(identity.fp())
            );
        }
    }

    #[test]
    fn signatures_for_another_host_or_nonce_are_rejected() {
        let identity = Identity::ec();
        let nonce = new_nonce().unwrap();
        let sig = identity.sign(&nonce, HOST);
        assert_eq!(
            verify(&identity.cert_b64(), &sig, &nonce, "evil.example"),
            None
        );
        assert_eq!(
            verify(&identity.cert_b64(), &sig, &new_nonce().unwrap(), HOST),
            None
        );
    }

    #[test]
    fn a_signature_by_another_key_is_rejected() {
        let (owner, thief) = (Identity::ec(), Identity::ec());
        let nonce = new_nonce().unwrap();
        let sig = thief.sign(&nonce, HOST);
        assert_eq!(verify(&owner.cert_b64(), &sig, &nonce, HOST), None);
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        let identity = Identity::ec();
        let nonce = new_nonce().unwrap();
        let sig = identity.sign(&nonce, HOST);
        assert_eq!(verify("not base64!", &sig, &nonce, HOST), None);
        assert_eq!(
            verify(&STANDARD.encode(b"not a cert"), &sig, &nonce, HOST),
            None
        );
        assert_eq!(verify(&identity.cert_b64(), "AAAA", &nonce, HOST), None);
        assert_eq!(verify(&identity.cert_b64(), "", &nonce, HOST), None);
    }

    #[test]
    fn trailing_bytes_after_the_certificate_are_rejected() {
        let identity = Identity::ec();
        let nonce = new_nonce().unwrap();
        let sig = identity.sign(&nonce, HOST);
        let mut padded = identity.cert_der.clone();
        padded.push(0);
        assert_eq!(verify(&STANDARD.encode(&padded), &sig, &nonce, HOST), None);
    }

    #[test]
    fn line_wrapped_base64_is_accepted() {
        let identity = Identity::ec();
        let nonce = new_nonce().unwrap();
        let wrapped: String = identity
            .cert_b64()
            .as_bytes()
            .chunks(76)
            .map(|line| format!("{}\n", String::from_utf8_lossy(line)))
            .collect();
        let sig = identity.sign(&nonce, HOST);
        assert_eq!(verify(&wrapped, &sig, &nonce, HOST), Some(identity.fp()));
    }

    #[test]
    fn nonce_is_32_random_bytes_in_base64url() {
        let nonce = new_nonce().unwrap();
        assert_eq!(URL_SAFE_NO_PAD.decode(&nonce).unwrap().len(), 32);
        assert_ne!(nonce, new_nonce().unwrap());
    }
}
