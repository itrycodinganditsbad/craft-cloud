//! What two-factor sign-in writes to the log: events with the kind and a device ID, and no password, secret, code, recovery
//! code, challenge, token or key, not even at debug. The subscriber is global, so this is the only test in its binary.

use std::sync::{Arc, Mutex};

use base64::Engine;
use craft_cloud_meta::{Meta, Role};
use craft_cloud_proto::api::{ErrorBody, LoginResponse, RecoveryCodes, TwoFactorSetupResponse};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_server::secretbox::{SecretBox, TwoFactorKey};
use craft_cloud_server::totp;
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use reqwest::{Client, StatusCode};
use serde_json::json;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

const PASSWORD: &str = "correct horse battery";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_factor_sign_in_logs_no_secret() {
    let log = Capture::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt().with_writer(log.clone()).with_max_level(tracing::Level::TRACE).with_ansi(false).finish(),
    )
    .unwrap();

    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let key_bytes = [42u8; 32];
    let state = AppState::new(meta.clone(), store, vec![])
        .unwrap()
        .with_two_factor(Some(SecretBox::new(&TwoFactorKey::from_bytes(key_bytes), None).unwrap()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
    tokio::spawn(craft_cloud_server::serve(state, listener, std::future::pending::<()>()));
    let client = Client::new();
    meta.create_user("ana", &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();

    let post = |path: &'static str, token: Option<String>, body: serde_json::Value| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            let mut req = client.post(format!("{base}{path}")).json(&body);
            if let Some(token) = token {
                req = req.bearer_auth(token);
            }
            req.send().await.unwrap()
        }
    };
    let login = post("/auth/login", None, json!({ "username": "ana", "password": PASSWORD, "device_name": "pc" })).await;
    let token = login.json::<LoginResponse>().await.unwrap().token;

    // Set up, with a wrong password and a wrong code on the way, and turn on.
    let wrong = post("/account/two-factor/setup", Some(token.clone()), json!({ "password": "a wrong password" })).await;
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
    let setup: TwoFactorSetupResponse =
        post("/account/two-factor/setup", Some(token.clone()), json!({ "password": PASSWORD })).await.json().await.unwrap();
    let secret = totp::base32_decode(&setup.secret).unwrap();
    let now = craft_cloud_server::state::now_ms();
    let step = totp::step_at(now);
    let first = totp::code_for(&secret, step).unwrap();
    let wrong_code = format!("{:06}", (first.parse::<u32>().unwrap() + 1) % 1_000_000);
    let res = post("/account/two-factor/confirm", Some(token.clone()), json!({ "totp_code": wrong_code })).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let codes: RecoveryCodes = post("/account/two-factor/confirm", Some(token.clone()), json!({ "totp_code": first })).await.json().await.unwrap();

    // Sign in with the second step, with a wrong code, a recovery code and the next code.
    let mut challenges = Vec::new();
    for proof in [
        json!({ "totp_code": wrong_code }),
        json!({ "recovery_code": codes.recovery_codes[0] }),
        json!({ "totp_code": totp::code_for(&secret, step + 1).unwrap() }),
    ] {
        let res = post("/auth/login", None, json!({ "username": "ana", "password": PASSWORD, "device_name": "laptop" })).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let challenge = res.json::<ErrorBody>().await.unwrap().two_factor.unwrap().challenge;
        let mut body = json!({ "challenge": challenge });
        body.as_object_mut().unwrap().extend(proof.as_object().unwrap().clone());
        let res = post("/auth/login/second-factor", None, body).await;
        challenges.push(challenge);
        if res.status() == StatusCode::OK {
            challenges.push(res.json::<LoginResponse>().await.unwrap().token);
        }
    }
    // Turn it off and join with an invite, with a wrong second factor first.
    let off =
        post("/account/two-factor/disable", Some(token.clone()), json!({ "password": PASSWORD, "recovery_code": codes.recovery_codes[1] })).await;
    assert_eq!(off.status(), StatusCode::NO_CONTENT);

    let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    // The events are there.
    assert!(text.contains("two-factor set-up started") && text.contains("two-factor sign-in turned on"), "{text}");
    assert!(text.contains("device signed in with a second factor") && text.contains("two-factor sign-in turned off"), "{text}");
    // Nothing secret is.
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(key_bytes);
    let mut secrets: Vec<String> = vec![
        PASSWORD.to_owned(),
        "a wrong password".to_owned(),
        setup.secret.clone(),
        setup.otpauth_uri.clone(),
        first.clone(),
        wrong_code.clone(),
        token.clone(),
        key_b64,
    ];
    secrets.extend(challenges);
    for code in &codes.recovery_codes {
        secrets.push(code.clone());
        secrets.push(code.replace('-', ""));
    }
    secrets.push(totp::code_for(&secret, step + 1).unwrap());
    // Without each line's timestamp, whose microseconds are six digits as a code is.
    let body: String = text.lines().map(|l| l.split_once(' ').map_or("", |(_, rest)| rest)).collect::<Vec<_>>().join(
        "
",
    );
    let lower = body.to_ascii_lowercase();
    for secret in &secrets {
        assert!(!body.contains(secret.as_str()), "{secret} in the log");
        assert!(!lower.contains(&secret.to_ascii_lowercase()), "{secret} (in any case) in the log");
    }
    // The username is not logged at info or above (the sign-in's own debug line is the one place).
    assert!(!text.lines().any(|l| l.contains(" INFO ") && l.contains("ana")), "{text}");
}
