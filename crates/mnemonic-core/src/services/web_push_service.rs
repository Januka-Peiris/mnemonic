//! Encrypted web push to the installed phone page (RFC 8291 and RFC 8292).

use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::Aead as _;
use aes_gcm::{Aes128Gcm, KeyInit as _};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer as _;
use p256::elliptic_curve::sec1::ToEncodedPoint as _;
use p256::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::services::team_share_service::is_plain_id;

const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const RECORD_SIZE: u32 = 4096;
const UNCOMPRESSED_POINT_LENGTH: u8 = 65;
// RFC 8291 section 4: one 4096-octet record minus the 86-octet header,
// the padding delimiter and the 16-octet tag.
const MAX_PLAINTEXT_LENGTH: usize = 3993;
const TOKEN_LIFETIME_SECONDS: u64 = 12 * 60 * 60;
const VAPID_SUBJECT: &str = "mailto:mnemonic@localhost";
const VAPID_KEY_FILE: &str = "vapid.key";
const MAX_DEVICE_TOKEN_BYTES: usize = 4096;
const MAX_DEVICE_NAME_CHARS: usize = 100;

/// Keys are base64url, as browsers send them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

impl PushSubscription {
    /// Whether pushes can be encrypted to this subscription's keys, so a
    /// browser's malformed keys are refused when it subscribes rather than
    /// failing every push later.
    pub fn check_keys(&self) -> Result<(), String> {
        decode_keys(self).map(|_| ())
    }
}

/// An installed app: a push token to send to, and keys the Mac seals the
/// alert with so the token's service never sees the contents.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushDevice {
    pub install_id: String,
    pub token: String,
    pub p256dh: String,
    pub auth: String,
    pub name: String,
}

impl std::fmt::Debug for PushDevice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PushDevice")
            .field("install_id", &self.install_id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl PushDevice {
    pub fn subscription(&self) -> PushSubscription {
        PushSubscription {
            endpoint: String::new(),
            p256dh: self.p256dh.clone(),
            auth: self.auth.clone(),
        }
    }

    pub fn check(&self) -> Result<(), String> {
        if !is_plain_id(&self.install_id) {
            return Err("The install id may only use letters, digits, - and _".to_string());
        }
        if self.token.is_empty() || self.token.len() > MAX_DEVICE_TOKEN_BYTES {
            return Err("The push token is missing or too long".to_string());
        }
        if self.name.chars().count() > MAX_DEVICE_NAME_CHARS {
            return Err("The device name is too long".to_string());
        }
        self.subscription().check_keys()
    }
}

/// Deliberately not `Debug`, so the private key can't end up in a log.
pub struct VapidKeys {
    signing_key: p256::ecdsa::SigningKey,
}

impl VapidKeys {
    pub fn public_key_base64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(
            self.signing_key
                .verifying_key()
                .to_encoded_point(false)
                .as_bytes(),
        )
    }
}

pub fn load_or_create_vapid_keys(directory: &Path) -> Result<VapidKeys, String> {
    let path = directory.join(VAPID_KEY_FILE);
    match std::fs::read_to_string(&path) {
        Ok(contents) => return parse_vapid_key(&contents, &path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("Couldn't read {}: {err}", path.display())),
    }

    std::fs::create_dir_all(directory)
        .map_err(|err| format!("Couldn't create {}: {err}", directory.display()))?;
    let private_key = random_private_key();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        // Another caller created the key first; use theirs so both agree.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let contents = std::fs::read_to_string(&path)
                .map_err(|err| format!("Couldn't read {}: {err}", path.display()))?;
            return parse_vapid_key(&contents, &path);
        }
        Err(err) => return Err(format!("Couldn't create {}: {err}", path.display())),
    };
    file.write_all(format!("{}\n", URL_SAFE_NO_PAD.encode(private_key)).as_bytes())
        .map_err(|err| format!("Couldn't write {}: {err}", path.display()))?;
    let signing_key = p256::ecdsa::SigningKey::from_bytes(&private_key)
        .map_err(|_| "The new push key isn't a valid P-256 key".to_string())?;
    Ok(VapidKeys { signing_key })
}

fn parse_vapid_key(contents: &str, path: &Path) -> Result<VapidKeys, String> {
    let invalid = || format!("{} doesn't hold a valid push key", path.display());
    let bytes = URL_SAFE_NO_PAD
        .decode(contents.trim())
        .map_err(|_| invalid())?;
    let signing_key = p256::ecdsa::SigningKey::from_bytes(&bytes).map_err(|_| invalid())?;
    Ok(VapidKeys { signing_key })
}

/// Rejection sampling: a random 32 bytes is a valid P-256 scalar unless it
/// is zero or at least the curve order, which is vanishingly rare.
fn random_private_key() -> [u8; 32] {
    loop {
        let candidate: [u8; 32] = rand::random();
        if SecretKey::from_be_bytes(&candidate).is_ok() {
            return candidate;
        }
    }
}

fn decode_base64url(value: &str, name: &str) -> Result<Vec<u8>, String> {
    URL_SAFE_NO_PAD
        .decode(value.trim().trim_end_matches('='))
        .map_err(|err| format!("The subscription's {name} isn't base64url: {err}"))
}

/// The browser's public key, an uncompressed P-256 point, and its 16-byte
/// auth secret.
fn decode_keys(subscription: &PushSubscription) -> Result<(PublicKey, Vec<u8>), String> {
    let receiver_public_bytes = decode_base64url(&subscription.p256dh, "p256dh key")?;
    if receiver_public_bytes.len() != usize::from(UNCOMPRESSED_POINT_LENGTH)
        || receiver_public_bytes.first() != Some(&4)
    {
        return Err("The subscription's p256dh key isn't an uncompressed P-256 point".to_string());
    }
    let receiver_public = PublicKey::from_sec1_bytes(&receiver_public_bytes)
        .map_err(|_| "The subscription's p256dh key isn't a P-256 point".to_string())?;
    let auth_secret = decode_base64url(&subscription.auth, "auth secret")?;
    if auth_secret.len() != 16 {
        return Err("The subscription's auth secret isn't 16 bytes".to_string());
    }
    Ok((receiver_public, auth_secret))
}

fn hkdf_expand<const LENGTH: usize>(
    salt: &[u8],
    input_key_material: &[u8],
    info: &[&[u8]],
) -> Result<[u8; LENGTH], String> {
    let mut output = [0u8; LENGTH];
    Hkdf::<Sha256>::new(Some(salt), input_key_material)
        .expand_multi_info(info, &mut output)
        .map_err(|err| format!("Couldn't derive the push key: {err}"))?;
    Ok(output)
}

/// RFC 8291 `aes128gcm` as a single record.
pub fn encrypt_with(
    plaintext: &[u8],
    subscription: &PushSubscription,
    server_private: &[u8; 32],
    salt: &[u8; 16],
) -> Result<Vec<u8>, String> {
    if plaintext.len() > MAX_PLAINTEXT_LENGTH {
        return Err(format!(
            "The push message is {} bytes; the most is {MAX_PLAINTEXT_LENGTH}",
            plaintext.len()
        ));
    }
    let (receiver_public, auth_secret) = decode_keys(subscription)?;
    let server_secret = SecretKey::from_be_bytes(server_private)
        .map_err(|_| "The push encryption key isn't a valid P-256 key".to_string())?;

    let receiver_public_point = receiver_public.to_encoded_point(false);
    let server_public_point = server_secret.public_key().to_encoded_point(false);
    let shared_secret = p256::ecdh::diffie_hellman(
        server_secret.to_nonzero_scalar(),
        receiver_public.as_affine(),
    );

    let input_key_material: [u8; 32] = hkdf_expand(
        &auth_secret,
        shared_secret.raw_secret_bytes(),
        &[
            b"WebPush: info\0",
            receiver_public_point.as_bytes(),
            server_public_point.as_bytes(),
        ],
    )?;
    let content_key: [u8; 16] = hkdf_expand(
        salt,
        &input_key_material,
        &[b"Content-Encoding: aes128gcm\0"],
    )?;
    let nonce: [u8; 12] = hkdf_expand(salt, &input_key_material, &[b"Content-Encoding: nonce\0"])?;

    let mut padded = Vec::with_capacity(plaintext.len() + 1);
    padded.extend_from_slice(plaintext);
    // 0x02 marks the last (and only) record.
    padded.push(2);
    let cipher = Aes128Gcm::new_from_slice(&content_key)
        .map_err(|err| format!("Couldn't set up push encryption: {err}"))?;
    let ciphertext = cipher
        .encrypt(&nonce.into(), padded.as_slice())
        .map_err(|err| format!("Couldn't encrypt the push message: {err}"))?;

    let mut body = Vec::new();
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(UNCOMPRESSED_POINT_LENGTH);
    body.extend_from_slice(server_public_point.as_bytes());
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

pub fn encrypt(plaintext: &[u8], subscription: &PushSubscription) -> Result<Vec<u8>, String> {
    let salt: [u8; 16] = rand::random();
    encrypt_with(plaintext, subscription, &random_private_key(), &salt)
}

/// RFC 8292 VAPID: an ES256 JWT whose audience is the push service's origin.
pub fn vapid_authorization(
    endpoint: &str,
    keys: &VapidKeys,
    now_unix: u64,
) -> Result<String, String> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|err| format!("The push endpoint isn't a URL: {err}"))?;
    if url.host_str().is_none() {
        return Err("The push endpoint has no host".to_string());
    }
    let audience = url.origin().ascii_serialization();
    let header = serde_json::json!({ "typ": "JWT", "alg": "ES256" });
    let claims = serde_json::json!({
        "aud": audience,
        "exp": now_unix + TOKEN_LIFETIME_SECONDS,
        "sub": VAPID_SUBJECT,
    });
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let signature: p256::ecdsa::Signature = keys.signing_key.sign(signing_input.as_bytes());
    let token = format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.as_ref() as &[u8])
    );
    Ok(format!(
        "vapid t={token}, k={}",
        keys.public_key_base64url()
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    Delivered,
    /// The push service no longer knows the subscription, so it should be
    /// forgotten.
    Gone,
}

fn outcome_for_status(status: u16) -> Option<PushOutcome> {
    match status {
        200..=299 => Some(PushOutcome::Delivered),
        404 | 410 => Some(PushOutcome::Gone),
        _ => None,
    }
}

pub fn send(
    subscription: &PushSubscription,
    payload: &[u8],
    keys: &VapidKeys,
) -> Result<PushOutcome, String> {
    let body = encrypt(payload, subscription)?;
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| format!("The clock is before 1970: {err}"))?
        .as_secs();
    let authorization = vapid_authorization(&subscription.endpoint, keys, now_unix)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(SEND_TIMEOUT)
        .build()
        .map_err(|err| format!("Couldn't set up the push request: {err}"))?;
    let response = client
        .post(&subscription.endpoint)
        .header("Content-Encoding", "aes128gcm")
        .header("TTL", "3600")
        .header("Urgency", "high")
        .header("Authorization", authorization)
        .body(body)
        .send()
        .map_err(|err| format!("Couldn't reach the push service: {err}"))?;
    let status = response.status();
    if let Some(outcome) = outcome_for_status(status.as_u16()) {
        return Ok(outcome);
    }
    let text = response
        .text()
        .unwrap_or_else(|err| format!("(couldn't read the answer: {err})"));
    Err(format!("The push service answered {status}: {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(text: &str) -> Result<Vec<u8>, String> {
        URL_SAFE_NO_PAD
            .decode(text)
            .map_err(|err| format!("bad base64url {text}: {err}"))
    }

    #[test]
    fn encrypts_the_rfc_8291_example() -> Result<(), String> {
        let subscription = PushSubscription {
            endpoint: "https://push.example.net/push/JzLQ3raZJfFBR0aqvOMsLrt54w4rJUsV".into(),
            p256dh: concat!(
                "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx",
                "aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"
            )
            .into(),
            auth: "BTBZMqHH6r4Tts7J_aSIgg".into(),
        };
        let server_private: [u8; 32] = decode("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")?
            .try_into()
            .map_err(|_| "server private key is not 32 bytes".to_string())?;
        let salt: [u8; 16] = decode("DGv6ra1nlYgDCS1FRnbzlw")?
            .try_into()
            .map_err(|_| "salt is not 16 bytes".to_string())?;
        let plaintext = decode("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24")?;
        assert_eq!(plaintext, b"When I grow up, I want to be a watermelon");
        let expected = decode(concat!(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml",
            "mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT",
            "pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        ))?;

        let body = encrypt_with(&plaintext, &subscription, &server_private, &salt)?;

        assert_eq!(
            URL_SAFE_NO_PAD.encode(&body),
            URL_SAFE_NO_PAD.encode(&expected)
        );
        Ok(())
    }

    #[test]
    fn android_fixture_matches_mac_encryption() -> Result<(), String> {
        let receiver_private = "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94";
        let receiver_public = concat!(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx",
            "aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"
        );
        let auth = "BTBZMqHH6r4Tts7J_aSIgg";
        let subscription = PushSubscription {
            endpoint: "https://fcm.invalid/fixture".into(),
            p256dh: receiver_public.into(),
            auth: auth.into(),
        };
        let server_private: [u8; 32] = decode("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")?
            .try_into()
            .map_err(|_| "server private key is not 32 bytes".to_string())?;
        let salt: [u8; 16] = decode("AAECAwQFBgcICQoLDA0ODw")?
            .try_into()
            .map_err(|_| "salt is not 16 bytes".to_string())?;
        let plaintext =
            r#"{"title":"mnemonic / fixture","body":"needs permission to run a command"}"#;

        let body = encrypt_with(plaintext.as_bytes(), &subscription, &server_private, &salt)?;

        let fixture = serde_json::json!({
            "receiverPrivate": receiver_private,
            "receiverPublic": receiver_public,
            "auth": auth,
            "body": URL_SAFE_NO_PAD.encode(&body),
            "plaintext": URL_SAFE_NO_PAD.encode(plaintext),
        });
        let rendered = format!(
            "{}\n",
            serde_json::to_string_pretty(&fixture).map_err(|err| err.to_string())?
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../android/app/src/test/resources/mac_push_fixture.json");
        if std::env::var("UPDATE_ANDROID_FIXTURES").as_deref() == Ok("1") {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
            }
            std::fs::write(&path, &rendered).map_err(|err| err.to_string())?;
        }
        let on_disk = std::fs::read_to_string(&path)
            .map_err(|err| format!("couldn't read {}: {err}", path.display()))?;
        assert_eq!(on_disk, rendered);
        Ok(())
    }

    #[test]
    fn vapid_token_names_the_push_service() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|err| err.to_string())?;
        let keys = load_or_create_vapid_keys(directory.path())?;

        let authorization =
            vapid_authorization("https://fcm.googleapis.com/fcm/send/abc", &keys, 1_000)?;

        let public_key = keys.public_key_base64url();
        let (token, key) = authorization
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split_once(", k="))
            .ok_or_else(|| format!("unexpected header {authorization}"))?;
        assert_eq!(key, public_key);

        let point = decode(&public_key)?;
        let (x, y) = point
            .get(1..33)
            .zip(point.get(33..65))
            .filter(|_| point.len() == 65 && point.first() == Some(&4))
            .ok_or_else(|| "public key is not an uncompressed P-256 point".to_string())?;
        let decoding_key = jsonwebtoken::DecodingKey::from_ec_components(
            &URL_SAFE_NO_PAD.encode(x),
            &URL_SAFE_NO_PAD.encode(y),
        )
        .map_err(|err| err.to_string())?;
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
        validation.set_audience(&["https://fcm.googleapis.com"]);
        validation.validate_exp = false;
        let decoded = jsonwebtoken::decode::<serde_json::Value>(token, &decoding_key, &validation)
            .map_err(|err| format!("token did not verify: {err}"))?;

        assert_eq!(decoded.claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(decoded.claims["exp"], 1_000 + 43_200);
        assert_eq!(decoded.claims["sub"], "mailto:mnemonic@localhost");
        Ok(())
    }

    #[test]
    fn vapid_key_is_private_and_kept_across_restarts() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|err| err.to_string())?;
        let first = load_or_create_vapid_keys(directory.path())?;
        let second = load_or_create_vapid_keys(directory.path())?;
        assert_eq!(first.public_key_base64url(), second.public_key_base64url());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let metadata = std::fs::metadata(directory.path().join("vapid.key"))
                .map_err(|err| err.to_string())?;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }

    #[test]
    fn gone_on_404_and_410() {
        assert_eq!(outcome_for_status(404), Some(PushOutcome::Gone));
        assert_eq!(outcome_for_status(410), Some(PushOutcome::Gone));
        assert_eq!(outcome_for_status(201), Some(PushOutcome::Delivered));
        assert_eq!(outcome_for_status(500), None);
    }
}
