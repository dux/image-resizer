// Stateless admin session: `base64url(json {email, exp}) . base64url(hmac_sha256(key, payload))`.
// The key lives in `DATA_DIR/session.key`; deleting it signs everyone out.

use std::fs;
use std::io::Write;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

pub const SESSION_COOKIE: &str = "ir_session";
pub const SESSION_DAYS: i64 = 30;

#[derive(Serialize, Deserialize)]
struct Payload {
    email: String,
    exp: i64,
}

pub struct SessionKey([u8; 32]);

impl SessionKey {
    /// Loads the key, creating it (0600) on first start.
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        if let Ok(bytes) = fs::read(path) {
            let key: [u8; 32] = bytes
                .try_into()
                .map_err(|_| format!("{} must hold 32 bytes", path.display()))?;
            return Ok(Self(key));
        }
        let key = crate::auth::random_bytes::<32>();
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        file.write_all(&key)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self(key))
    }

    #[cfg(test)]
    pub fn from_bytes(key: [u8; 32]) -> Self {
        Self(key)
    }

    fn mac(&self, payload: &str) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("hmac accepts any key length");
        mac.update(payload.as_bytes());
        mac
    }

    pub fn sign(&self, email: &str, exp: i64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&Payload {
                email: email.into(),
                exp,
            })
            .unwrap_or_default(),
        );
        let sig = URL_SAFE_NO_PAD.encode(self.mac(&payload).finalize().into_bytes());
        format!("{payload}.{sig}")
    }

    /// The email of a valid, unexpired cookie value.
    pub fn verify(&self, value: &str, now: i64) -> Option<String> {
        let (payload, sig) = value.split_once('.')?;
        let sig = URL_SAFE_NO_PAD.decode(sig).ok()?;
        self.mac(payload).verify_slice(&sig).ok()?;
        let payload: Payload =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        (payload.exp > now).then_some(payload.email)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_expire_and_tamper() {
        let key = SessionKey::from_bytes([7; 32]);
        let value = key.sign("rejotl@gmail.com", 100);
        assert_eq!(key.verify(&value, 99).as_deref(), Some("rejotl@gmail.com"));
        assert_eq!(key.verify(&value, 100), None, "expired");
        assert_eq!(
            SessionKey::from_bytes([8; 32]).verify(&value, 0),
            None,
            "other key"
        );
        let (_, sig) = value.split_once('.').unwrap();
        let forged = format!(
            "{}.{sig}",
            URL_SAFE_NO_PAD.encode(br#"{"email":"evil@x.com","exp":999}"#)
        );
        assert_eq!(key.verify(&forged, 0), None, "forged payload");
        assert_eq!(key.verify("garbage", 0), None);
    }

    #[test]
    fn key_file_is_created_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.key");
        let a = SessionKey::load_or_create(&path).unwrap();
        let b = SessionKey::load_or_create(&path).unwrap();
        assert_eq!(a.0, b.0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
