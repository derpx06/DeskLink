use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, PKeyRef, Private, Public};
use openssl::sign::{Signer, Verifier};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticationTranscript {
    pub session_attempt_id: String,
    pub initiator_device_id: String,
    pub responder_device_id: String,
    pub session_id: u64,
    pub connection_generation: u64,
    pub initiator_nonce: String,
    pub responder_nonce: String,
    pub offer_sha256: String,
    pub answer_sha256: String,
    pub initiator_dtls_fingerprint: String,
    pub responder_dtls_fingerprint: String,
    pub protocol_version: u8,
    pub timestamp: i64,
}

impl AuthenticationTranscript {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    pub fn sign(&self, private_key_pem: &[u8]) -> Result<Vec<u8>, AuthenticationError> {
        let key = PKey::<Private>::private_key_from_pem(private_key_pem)
            .map_err(|error| AuthenticationError::Key(error.to_string()))?;
        self.sign_with_key(&key)
    }

    pub fn sign_with_key(&self, key: &PKeyRef<Private>) -> Result<Vec<u8>, AuthenticationError> {
        let mut signer = Signer::new(MessageDigest::sha256(), key)
            .map_err(|error| AuthenticationError::Crypto(error.to_string()))?;
        signer
            .update(
                &self
                    .canonical_bytes()
                    .map_err(|_| AuthenticationError::Malformed)?,
            )
            .map_err(|error| AuthenticationError::Crypto(error.to_string()))?;
        signer
            .sign_to_vec()
            .map_err(|error| AuthenticationError::Crypto(error.to_string()))
    }

    pub fn verify(
        &self,
        public_key_pem: &[u8],
        signature: &[u8],
    ) -> Result<(), AuthenticationError> {
        let key = PKey::<Public>::public_key_from_pem(public_key_pem)
            .map_err(|error| AuthenticationError::Key(error.to_string()))?;
        self.verify_with_key(&key, signature)
    }

    pub fn verify_with_key(
        &self,
        key: &PKeyRef<Public>,
        signature: &[u8],
    ) -> Result<(), AuthenticationError> {
        let mut verifier = Verifier::new(MessageDigest::sha256(), key)
            .map_err(|error| AuthenticationError::Crypto(error.to_string()))?;
        verifier
            .update(
                &self
                    .canonical_bytes()
                    .map_err(|_| AuthenticationError::Malformed)?,
            )
            .map_err(|error| AuthenticationError::Crypto(error.to_string()))?;
        if verifier
            .verify(signature)
            .map_err(|error| AuthenticationError::Crypto(error.to_string()))?
        {
            Ok(())
        } else {
            Err(AuthenticationError::InvalidSignature)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthenticationError {
    Key(String),
    Crypto(String),
    InvalidSignature,
    Malformed,
}

impl std::fmt::Display for AuthenticationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WebRTC authentication error: {self:?}")
    }
}
impl std::error::Error for AuthenticationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::ec::{EcGroup, EcKey};
    use openssl::nid::Nid;

    fn transcript() -> AuthenticationTranscript {
        AuthenticationTranscript {
            session_attempt_id: "attempt".into(),
            initiator_device_id: "desktop".into(),
            responder_device_id: "phone".into(),
            session_id: 4,
            connection_generation: 2,
            initiator_nonce: "a".into(),
            responder_nonce: "b".into(),
            offer_sha256: "offer".into(),
            answer_sha256: "answer".into(),
            initiator_dtls_fingerprint: "one".into(),
            responder_dtls_fingerprint: "two".into(),
            protocol_version: 1,
            timestamp: 1,
        }
    }

    #[test]
    fn p256_transcript_signature_is_bound_to_all_fields() {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let key = EcKey::generate(&group).unwrap();
        let private = PKey::from_ec_key(key.clone()).unwrap();
        let public = PKey::from_ec_key(key).unwrap();
        let private_pem = private.private_key_to_pem_pkcs8().unwrap();
        let public_pem = public.public_key_to_pem().unwrap();
        let mut changed = transcript();
        let signature = transcript().sign(&private_pem).unwrap();
        transcript().verify(&public_pem, &signature).unwrap();
        changed.connection_generation += 1;
        assert_eq!(
            changed.verify(&public_pem, &signature),
            Err(AuthenticationError::InvalidSignature)
        );
    }

    #[test]
    fn canonical_transcript_matches_android_field_order() {
        assert_eq!(
            String::from_utf8(transcript().canonical_bytes().unwrap()).unwrap(),
            r#"{"sessionAttemptId":"attempt","initiatorDeviceId":"desktop","responderDeviceId":"phone","sessionId":4,"connectionGeneration":2,"initiatorNonce":"a","responderNonce":"b","offerSha256":"offer","answerSha256":"answer","initiatorDtlsFingerprint":"one","responderDtlsFingerprint":"two","protocolVersion":1,"timestamp":1}"#
        );
    }
}
