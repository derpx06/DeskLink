//! The end-to-end channel two paired devices run *through* the relay.
//!
//! The relay forwards opaque JSON and stamps who sent it, but it is operated
//! by someone other than the user, so nothing it carries may be readable or
//! forgeable by it. Each side proves its identity with the key behind its
//! pairing certificate — verified against the certificate *stored at pairing*,
//! never one the relay supplies — over a transcript that binds both ephemeral
//! keys, both nonces and both identity packets. The ECDH secret then keys
//! AES-256-GCM in each direction.
//!
//! Wire shapes (inside the relay's `data`), with `I` the device whose relay
//! fingerprint sorts lower and `R` the other:
//!
//! ```text
//! I→R {"k":"h1","lv":1,"eph":B64,"nonce":B64,"identity":"<identity json>"}
//! R→I {"k":"h2","lv":1,"eph":B64,"nonce":B64,"identity":"<identity json>","sig":B64}
//! I→R {"k":"fin","sig":B64}
//! both {"k":"d","seq":N,"ct":B64}      N = 1, 2, … per direction
//! R→I {"k":"kick"}                      "start a handshake"
//! ```
//!
//! `eph` is an uncompressed P-256 point (65 bytes), `nonce` 32 random bytes,
//! B64 the standard alphabet with padding. Signatures are SHA-256 with the
//! device key (ECDSA DER or RSA PKCS#1 v1.5) over `"responder\n"` or
//! `"initiator\n"` followed by [`transcript`]. Keys: HKDF-SHA256 with
//! salt = nonceI‖nonceR, ikm = the ECDH x-coordinate, info =
//! `"desklink-relay-link-v1 keys\n" + fpI + "\n" + fpR`, 64 bytes: the first
//! 32 key I→R, the last 32 R→I. A frame's IV is four zero bytes then `seq` as
//! a big-endian u64; its AAD is [`DATA_AAD`]. The plaintext is one DeskLink
//! packet, serialized without its trailing newline.
//!
//! Android implements the same bytes (`RelayLinkCrypto.kt`); the vector in
//! this module's tests is pinned on both sides.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
#[cfg(test)]
use openssl::bn::BigNum;
use openssl::bn::BigNumContext;
use openssl::derive::Deriver;
use openssl::ec::{EcGroup, EcKey, EcPoint, PointConversionForm};
use openssl::hash::{hash, MessageDigest};
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private, Public};
use openssl::sign::{Signer, Verifier};
use openssl::symm::{decrypt_aead, encrypt_aead, Cipher};

pub const LINK_VERSION: u32 = 1;
const CONTEXT: &str = "desklink-relay-link-v1";
pub const DATA_AAD: &[u8] = b"desklink-relay-link-v1 data";
const TAG_BYTES: usize = 16;

/// A device's relay address: base64url, unpadded, of SHA-256(certificate DER).
pub fn fingerprint(certificate_der: &[u8]) -> String {
    let digest = hash(MessageDigest::sha256(), certificate_der)
        .map(|digest| digest.to_vec())
        .unwrap_or_default();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Whether the device with `local_fp` opens the handshake with `remote_fp`.
pub fn is_initiator(local_fp: &str, remote_fp: &str) -> bool {
    local_fp < remote_fp
}

/// One side's ephemeral contribution.
pub struct Ephemeral {
    key: PKey<Private>,
    pub public: Vec<u8>,
    pub nonce: [u8; 32],
}

impl Ephemeral {
    pub fn generate() -> Result<Self, String> {
        let group = p256()?;
        let key = EcKey::generate(&group).map_err(err)?;
        let mut nonce = [0u8; 32];
        openssl::rand::rand_bytes(&mut nonce).map_err(err)?;
        Self::from_ec(key, nonce)
    }

    /// For the pinned test vector only.
    #[cfg(test)]
    pub fn from_scalar(scalar: &[u8], nonce: [u8; 32]) -> Result<Self, String> {
        let group = p256()?;
        let private = BigNum::from_slice(scalar).map_err(err)?;
        let mut public = EcPoint::new(&group).map_err(err)?;
        let mut context = BigNumContext::new().map_err(err)?;
        public
            .mul_generator2(&group, &private, &mut context)
            .map_err(err)?;
        let key = EcKey::from_private_components(&group, &private, &public).map_err(err)?;
        Self::from_ec(key, nonce)
    }

    fn from_ec(key: EcKey<Private>, nonce: [u8; 32]) -> Result<Self, String> {
        let group = p256()?;
        let mut context = BigNumContext::new().map_err(err)?;
        let public = key
            .public_key()
            .to_bytes(&group, PointConversionForm::UNCOMPRESSED, &mut context)
            .map_err(err)?;
        Ok(Self {
            key: PKey::from_ec_key(key).map_err(err)?,
            public,
            nonce,
        })
    }
}

/// Everything both signatures cover. Field order and separators are part of
/// the protocol.
pub struct Transcript<'a> {
    pub fp_initiator: &'a str,
    pub fp_responder: &'a str,
    pub eph_initiator: &'a [u8],
    pub eph_responder: &'a [u8],
    pub nonce_initiator: &'a [u8],
    pub nonce_responder: &'a [u8],
    pub identity_initiator: &'a str,
    pub identity_responder: &'a str,
}

pub fn transcript(t: &Transcript<'_>) -> Vec<u8> {
    let identity_hash = |identity: &str| {
        hash(MessageDigest::sha256(), identity.as_bytes())
            .map(|digest| hex(&digest))
            .unwrap_or_default()
    };
    [
        CONTEXT.to_string(),
        t.fp_initiator.to_string(),
        t.fp_responder.to_string(),
        BASE64.encode(t.eph_initiator),
        BASE64.encode(t.eph_responder),
        BASE64.encode(t.nonce_initiator),
        BASE64.encode(t.nonce_responder),
        identity_hash(t.identity_initiator),
        identity_hash(t.identity_responder),
    ]
    .join("\n")
    .into_bytes()
}

/// Which side a signature speaks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Responder,
}

impl Role {
    fn label(self) -> &'static [u8] {
        match self {
            Role::Initiator => b"initiator\n",
            Role::Responder => b"responder\n",
        }
    }
}

pub fn sign(key: &PKey<Private>, role: Role, transcript: &[u8]) -> Result<String, String> {
    let mut signer = Signer::new(MessageDigest::sha256(), key).map_err(err)?;
    signer.update(role.label()).map_err(err)?;
    signer.update(transcript).map_err(err)?;
    Ok(BASE64.encode(signer.sign_to_vec().map_err(err)?))
}

pub fn verify(key: &PKey<Public>, role: Role, transcript: &[u8], signature: &str) -> bool {
    let Ok(signature) = BASE64.decode(signature) else {
        return false;
    };
    let Ok(mut verifier) = Verifier::new(MessageDigest::sha256(), key) else {
        return false;
    };
    verifier
        .update(role.label())
        .and_then(|_| verifier.update(transcript))
        .and_then(|_| verifier.verify(&signature))
        .unwrap_or(false)
}

/// The two directional keys, from this side's ephemeral and the peer's.
pub struct LinkKeys {
    pub send: [u8; 32],
    pub receive: [u8; 32],
}

pub fn derive_keys(
    local: &Ephemeral,
    peer_public: &[u8],
    role: Role,
    fp_initiator: &str,
    fp_responder: &str,
    nonce_initiator: &[u8],
    nonce_responder: &[u8],
) -> Result<LinkKeys, String> {
    let group = p256()?;
    let mut context = BigNumContext::new().map_err(err)?;
    let point = EcPoint::from_bytes(&group, peer_public, &mut context)
        .map_err(|_| "Invalid relay link key".to_string())?;
    let peer =
        PKey::from_ec_key(EcKey::from_public_key(&group, &point).map_err(err)?).map_err(err)?;
    let mut deriver = Deriver::new(&local.key).map_err(err)?;
    deriver.set_peer(&peer).map_err(err)?;
    let secret = deriver.derive_to_vec().map_err(err)?;

    let salt = [nonce_initiator, nonce_responder].concat();
    let info = format!("{CONTEXT} keys\n{fp_initiator}\n{fp_responder}");
    let okm = hkdf_sha256(&salt, &secret, info.as_bytes(), 64)?;
    let mut initiator_to_responder = [0u8; 32];
    let mut responder_to_initiator = [0u8; 32];
    initiator_to_responder.copy_from_slice(&okm[..32]);
    responder_to_initiator.copy_from_slice(&okm[32..]);
    Ok(match role {
        Role::Initiator => LinkKeys {
            send: initiator_to_responder,
            receive: responder_to_initiator,
        },
        Role::Responder => LinkKeys {
            send: responder_to_initiator,
            receive: initiator_to_responder,
        },
    })
}

/// RFC 5869.
fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], length: usize) -> Result<Vec<u8>, String> {
    let hmac = |key: &[u8], parts: &[&[u8]]| -> Result<Vec<u8>, String> {
        let key = PKey::hmac(key).map_err(err)?;
        let mut signer = Signer::new(MessageDigest::sha256(), &key).map_err(err)?;
        for part in parts {
            signer.update(part).map_err(err)?;
        }
        signer.sign_to_vec().map_err(err)
    };
    let prk = hmac(salt, &[ikm])?;
    let mut okm = Vec::with_capacity(length);
    let mut block = Vec::new();
    let mut counter = 1u8;
    while okm.len() < length {
        block = hmac(&prk, &[&block, info, &[counter]])?;
        okm.extend_from_slice(&block);
        counter = counter.checked_add(1).ok_or("HKDF output too long")?;
    }
    okm.truncate(length);
    Ok(okm)
}

fn iv(seq: u64) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[4..].copy_from_slice(&seq.to_be_bytes());
    iv
}

pub fn seal(key: &[u8; 32], seq: u64, plaintext: &[u8]) -> Result<String, String> {
    let mut tag = [0u8; TAG_BYTES];
    let mut sealed = encrypt_aead(
        Cipher::aes_256_gcm(),
        key,
        Some(&iv(seq)),
        DATA_AAD,
        plaintext,
        &mut tag,
    )
    .map_err(err)?;
    sealed.extend_from_slice(&tag);
    Ok(BASE64.encode(sealed))
}

pub fn open(key: &[u8; 32], seq: u64, sealed: &str) -> Result<Vec<u8>, String> {
    let sealed = BASE64
        .decode(sealed)
        .map_err(|_| "Malformed relay frame".to_string())?;
    if sealed.len() < TAG_BYTES {
        return Err("Malformed relay frame".to_string());
    }
    let (ciphertext, tag) = sealed.split_at(sealed.len() - TAG_BYTES);
    decrypt_aead(
        Cipher::aes_256_gcm(),
        key,
        Some(&iv(seq)),
        DATA_AAD,
        ciphertext,
        tag,
    )
    .map_err(|_| "Relay frame failed authentication".to_string())
}

fn p256() -> Result<EcGroup, String> {
    EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).map_err(err)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The cross-language vector. Android's `RelayLinkCryptoTest` pins the
    /// same inputs and outputs; change both or neither.
    pub(crate) const FP_I: &str = "AAAAinitiator";
    pub(crate) const FP_R: &str = "ZZZZresponder";
    pub(crate) const SCALAR_I: [u8; 32] = [0x11; 32];
    pub(crate) const SCALAR_R: [u8; 32] = [0x22; 32];
    pub(crate) const NONCE_I: [u8; 32] = [0x33; 32];
    pub(crate) const NONCE_R: [u8; 32] = [0x44; 32];
    pub(crate) const IDENTITY_I: &str = r#"{"deviceId":"phone"}"#;
    pub(crate) const IDENTITY_R: &str = r#"{"deviceId":"desktop"}"#;
    pub(crate) const PLAINTEXT: &[u8] = br#"{"type":"desklink.heartbeat"}"#;

    fn vector() -> (Ephemeral, Ephemeral) {
        (
            Ephemeral::from_scalar(&SCALAR_I, NONCE_I).unwrap(),
            Ephemeral::from_scalar(&SCALAR_R, NONCE_R).unwrap(),
        )
    }

    #[test]
    fn the_pinned_vector_derives_the_same_keys_on_both_sides() {
        let (i, r) = vector();
        let on_i = derive_keys(
            &i,
            &r.public,
            Role::Initiator,
            FP_I,
            FP_R,
            &NONCE_I,
            &NONCE_R,
        )
        .unwrap();
        let on_r = derive_keys(
            &r,
            &i.public,
            Role::Responder,
            FP_I,
            FP_R,
            &NONCE_I,
            &NONCE_R,
        )
        .unwrap();
        assert_eq!(on_i.send, on_r.receive);
        assert_eq!(on_i.receive, on_r.send);

        let transcript = transcript(&Transcript {
            fp_initiator: FP_I,
            fp_responder: FP_R,
            eph_initiator: &i.public,
            eph_responder: &r.public,
            nonce_initiator: &NONCE_I,
            nonce_responder: &NONCE_R,
            identity_initiator: IDENTITY_I,
            identity_responder: IDENTITY_R,
        });
        let digest = |bytes: &[u8]| hex(&hash(MessageDigest::sha256(), bytes).unwrap());
        // Pinned outputs (also in RelayLinkCryptoTest.kt).
        assert_eq!(
            hex(&i.public[..9]),
            VECTOR_EPH_I_PREFIX,
            "initiator public key"
        );
        assert_eq!(digest(&transcript), VECTOR_TRANSCRIPT_SHA256);
        assert_eq!(hex(&on_i.send), VECTOR_KEY_I_TO_R);
        assert_eq!(hex(&on_i.receive), VECTOR_KEY_R_TO_I);
        assert_eq!(seal(&on_i.send, 1, PLAINTEXT).unwrap(), VECTOR_FRAME_SEQ_1);
    }

    pub(crate) const VECTOR_EPH_I_PREFIX: &str = "040217e617f0b64439";
    pub(crate) const VECTOR_TRANSCRIPT_SHA256: &str =
        "b9ddecf9c6f46eaac3e6da802ad29be685b6ab572f38b3bdb633404b0a3aa10e";
    pub(crate) const VECTOR_KEY_I_TO_R: &str =
        "780a247c92688ebe8dfd0fd01d527cd6bdcd9839f2dd6bb4ff689aaa901a4fcf";
    pub(crate) const VECTOR_KEY_R_TO_I: &str =
        "058d8286624438b81cd9f2ce428b95ce9e4bce9bc97680c93bd4c1cf2d2404ec";
    pub(crate) const VECTOR_FRAME_SEQ_1: &str =
        "XBeya5M7xNZuQIiUpgn7aeVXgEVd4msLt1o9q/OUk3x0LpOVLZUm8KsVA8cm";

    #[test]
    fn frames_open_only_with_the_right_key_and_sequence() {
        let (i, r) = vector();
        let keys = derive_keys(
            &i,
            &r.public,
            Role::Initiator,
            FP_I,
            FP_R,
            &NONCE_I,
            &NONCE_R,
        )
        .unwrap();
        let frame = seal(&keys.send, 7, PLAINTEXT).unwrap();
        assert_eq!(open(&keys.send, 7, &frame).unwrap(), PLAINTEXT);
        assert!(open(&keys.send, 8, &frame).is_err());
        assert!(open(&keys.receive, 7, &frame).is_err());
        let mut tampered = BASE64.decode(&frame).unwrap();
        tampered[0] ^= 1;
        assert!(open(&keys.send, 7, &BASE64.encode(tampered)).is_err());
    }

    #[test]
    fn signatures_bind_the_role_and_the_transcript() {
        let group = p256().unwrap();
        let key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
        let public = PKey::public_key_from_der(&key.public_key_to_der().unwrap()).unwrap();
        let signature = sign(&key, Role::Responder, b"t").unwrap();
        assert!(verify(&public, Role::Responder, b"t", &signature));
        assert!(!verify(&public, Role::Initiator, b"t", &signature));
        assert!(!verify(&public, Role::Responder, b"u", &signature));

        let rsa = PKey::from_rsa(openssl::rsa::Rsa::generate(2048).unwrap()).unwrap();
        let rsa_public = PKey::public_key_from_der(&rsa.public_key_to_der().unwrap()).unwrap();
        let signature = sign(&rsa, Role::Initiator, b"t").unwrap();
        assert!(verify(&rsa_public, Role::Initiator, b"t", &signature));
    }

    #[test]
    fn hkdf_matches_rfc_5869_test_case_1() {
        let ikm = [0x0b; 22];
        let salt: Vec<u8> = (0x00..=0x0c).collect();
        let info: Vec<u8> = (0xf0..=0xf9).collect();
        assert_eq!(
            hex(&hkdf_sha256(&salt, &ikm, &info, 42).unwrap()),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }
}
