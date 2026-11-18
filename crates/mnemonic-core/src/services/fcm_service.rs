//! Alerts to the Android app through Firebase Cloud Messaging (HTTP v1).

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;

use crate::services::web_push_service::{self, PushDevice};

const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const MESSAGING_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const ASSERTION_LIFETIME_SECONDS: u64 = 3600;
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// `Debug` is hand-written to leave out the private key.
#[derive(Deserialize)]
pub struct ServiceAccount {
    pub project_id: String,
    pub client_email: String,
    pub private_key: String,
    pub token_uri: String,
}

impl std::fmt::Debug for ServiceAccount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceAccount")
            .field("project_id", &self.project_id)
            .field("client_email", &self.client_email)
            .finish_non_exhaustive()
    }
}

impl ServiceAccount {
    pub fn load(path: &Path) -> Result<ServiceAccount, String> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "The Firebase key file is missing: {}",
                    path.display()
                ));
            }
            Err(err) => return Err(format!("The Firebase key file couldn't be read: {err}")),
        };
        serde_json::from_str(&contents)
            .map_err(|err| format!("The Firebase key file couldn't be read: {err}"))
    }
}

pub fn jwt_assertion(account: &ServiceAccount, now_seconds: u64) -> Result<String, String> {
    let claims = serde_json::json!({
        "iss": account.client_email,
        "scope": MESSAGING_SCOPE,
        "aud": account.token_uri,
        "iat": now_seconds,
        "exp": now_seconds + ASSERTION_LIFETIME_SECONDS,
    });
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(account.private_key.as_bytes())
        .map_err(|err| format!("The Firebase key file has an unusable private key: {err}"))?;
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &key,
    )
    .map_err(|err| format!("Couldn't sign the Firebase request: {err}"))
}

struct CachedToken {
    client_email: String,
    token: String,
    refresh_at: Instant,
}

pub struct AccessTokenCache {
    cached: Mutex<Option<CachedToken>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

impl AccessTokenCache {
    pub fn shared() -> &'static AccessTokenCache {
        static SHARED: OnceLock<AccessTokenCache> = OnceLock::new();
        SHARED.get_or_init(|| AccessTokenCache {
            cached: Mutex::new(None),
        })
    }

    pub fn token(&self, account: &ServiceAccount) -> Result<String, String> {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let still_valid = cached.as_ref().filter(|cached| {
            cached.client_email == account.client_email && Instant::now() < cached.refresh_at
        });
        if let Some(cached) = still_valid {
            return Ok(cached.token.clone());
        }
        let fetched = fetch_access_token(account)?;
        let lifetime = Duration::from_secs(fetched.expires_in);
        *cached = Some(CachedToken {
            client_email: account.client_email.clone(),
            token: fetched.access_token.clone(),
            refresh_at: Instant::now() + lifetime.saturating_sub(REFRESH_MARGIN),
        });
        Ok(fetched.access_token)
    }
}

fn fetch_access_token(account: &ServiceAccount) -> Result<TokenResponse, String> {
    let now_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|err| format!("The clock is before 1970: {err}"))?
        .as_secs();
    let assertion = jwt_assertion(account, now_seconds)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(SEND_TIMEOUT)
        .build()
        .map_err(|err| format!("Couldn't set up the Firebase request: {err}"))?;
    let response = client
        .post(&account.token_uri)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ])
        .send()
        .map_err(|err| format!("Couldn't reach Google to sign in to Firebase: {err}"))?;
    let status = response.status();
    let text = response
        .text()
        .map_err(|err| format!("Couldn't read Google's answer: {err}"))?;
    if !status.is_success() {
        return Err(format!(
            "Google refused the Firebase sign-in, {status}: {text}"
        ));
    }
    serde_json::from_str(&text).map_err(|err| format!("Google's sign-in answer was odd: {err}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FcmOutcome {
    Delivered,
    /// Firebase no longer knows the token, so the device should be forgotten.
    Unregistered,
}

pub fn outcome_for_status(status: u16, body: &str) -> Result<FcmOutcome, String> {
    match status {
        200..=299 => Ok(FcmOutcome::Delivered),
        404 => Ok(FcmOutcome::Unregistered),
        _ if body.contains("UNREGISTERED") => Ok(FcmOutcome::Unregistered),
        _ => Err(format!("Firebase answered {status}: {body}")),
    }
}

pub fn send(
    account: &ServiceAccount,
    access_token: &str,
    device: &PushDevice,
    payload: &[u8],
) -> Result<FcmOutcome, String> {
    let ciphertext = web_push_service::encrypt(payload, &device.subscription())?;
    let body = serde_json::json!({
        "message": {
            "token": device.token,
            "data": { "payload": URL_SAFE_NO_PAD.encode(ciphertext) },
            "android": { "priority": "HIGH" },
        }
    });
    let client = reqwest::blocking::Client::builder()
        .timeout(SEND_TIMEOUT)
        .build()
        .map_err(|err| format!("Couldn't set up the Firebase request: {err}"))?;
    let response = client
        .post(format!(
            "https://fcm.googleapis.com/v1/projects/{}/messages:send",
            account.project_id
        ))
        .bearer_auth(access_token)
        .json(&body)
        .send()
        .map_err(|err| format!("Couldn't reach Firebase: {err}"))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .unwrap_or_else(|err| format!("(couldn't read the answer: {err})"));
    outcome_for_status(status, &text)
}

pub fn fcm_status(path: Option<&Path>) -> Result<(), String> {
    let path = path.ok_or_else(|| "No Firebase key file is set".to_string())?;
    ServiceAccount::load(path).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_claims_are_what_google_expects() -> Result<(), String> {
        let key_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fcm_test_key.pem");
        let account = ServiceAccount {
            project_id: "demo".into(),
            client_email: "sender@demo.iam.gserviceaccount.com".into(),
            private_key: std::fs::read_to_string(&key_path)
                .map_err(|err| format!("{}: {err}", key_path.display()))?,
            token_uri: "https://oauth2.googleapis.com/token".into(),
        };
        let assertion = jwt_assertion(&account, 1_000)?;
        let mut parts = assertion.split('.');
        let (Some(header), Some(claims), Some(_signature)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(format!("not a JWT: {assertion}"));
        };
        let decode = |part: &str| -> Result<serde_json::Value, String> {
            let bytes = URL_SAFE_NO_PAD
                .decode(part)
                .map_err(|err| err.to_string())?;
            serde_json::from_slice(&bytes).map_err(|err| err.to_string())
        };
        assert_eq!(decode(header)?["alg"], "RS256");
        let claims = decode(claims)?;
        assert_eq!(claims["iss"], "sender@demo.iam.gserviceaccount.com");
        assert_eq!(
            claims["scope"],
            "https://www.googleapis.com/auth/firebase.messaging"
        );
        assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
        assert_eq!(claims["iat"], 1_000);
        assert_eq!(claims["exp"], 4_600);
        Ok(())
    }

    #[test]
    fn unregistered_answer_means_forget_device() {
        assert_eq!(outcome_for_status(404, ""), Ok(FcmOutcome::Unregistered));
        assert_eq!(
            outcome_for_status(
                400,
                "{\"error\":{\"details\":[{\"errorCode\":\"UNREGISTERED\"}]}}"
            ),
            Ok(FcmOutcome::Unregistered)
        );
        assert!(outcome_for_status(500, "x").is_err());
        assert_eq!(outcome_for_status(200, ""), Ok(FcmOutcome::Delivered));
    }

    #[test]
    fn fcm_status_reports_missing_file() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|err| err.to_string())?;
        let missing = directory.path().join("nope.json");
        let err = fcm_status(Some(&missing)).err().unwrap_or_default();
        assert!(
            err.starts_with("The Firebase key file is missing: "),
            "{err}"
        );
        assert_eq!(
            fcm_status(None),
            Err("No Firebase key file is set".to_string())
        );
        Ok(())
    }
}
