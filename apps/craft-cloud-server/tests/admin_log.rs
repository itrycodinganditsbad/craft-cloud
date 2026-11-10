//! What the admin API writes to the log: events with the kind and the target's ID, and no password, invite or reset
//! code, join link or token. The subscriber is global, so this is the only test in its binary.

use std::sync::{Arc, Mutex};

use craft_cloud_meta::{Meta, Role};
use craft_cloud_proto::api::{AdminCode, AdminCreatedUser, LoginResponse};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
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
async fn the_admin_api_logs_no_secret() {
    let log = Capture::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt().with_writer(log.clone()).with_max_level(tracing::Level::DEBUG).with_ansi(false).finish(),
    )
    .unwrap();

    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let state = AppState::new(meta.clone(), store, vec![]).unwrap().with_public_url(Some("https://cloud.example.org".to_owned()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
    tokio::spawn(craft_cloud_server::serve(state, listener, std::future::pending::<()>()));
    let client = Client::new();
    meta.create_user("root", &hash_password(PASSWORD).unwrap(), Role::Admin, 0).await.unwrap();
    meta.create_user("ana", &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();
    let ana_id = meta.user_by_name("ana").await.unwrap().unwrap().id.0;

    let login = client
        .post(format!("{base}/auth/login"))
        .json(&json!({ "username": "root", "password": PASSWORD, "device_name": "pc" }))
        .send()
        .await
        .unwrap();
    let token = login.json::<LoginResponse>().await.unwrap().token;
    let post = |path: String, body: serde_json::Value| {
        let (client, token, base) = (client.clone(), token.clone(), base.clone());
        async move { client.post(format!("{base}{path}")).bearer_auth(token).json(&body).send().await.unwrap() }
    };

    // A wrong password, a new account with its invite, a reset code redeemed with a new password.
    let wrong = post("/admin/users".into(), json!({ "username": "newbie", "role": "member", "password": "a wrong password" })).await;
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
    let made: AdminCreatedUser =
        post("/admin/users".into(), json!({ "username": "newbie", "role": "member", "password": PASSWORD })).await.json().await.unwrap();
    let reset: AdminCode = post(format!("/admin/users/{ana_id}/password-reset"), json!({ "password": PASSWORD })).await.json().await.unwrap();
    let new_password = "a brand new secret one";
    let joined = client
        .post(format!("{base}/join"))
        .json(&json!({ "code": reset.code, "device_name": "recovery", "password": new_password }))
        .send()
        .await
        .unwrap();
    assert_eq!(joined.status(), StatusCode::OK);
    let new_token = joined.json::<LoginResponse>().await.unwrap().token;
    let disabled = post(format!("/admin/users/{ana_id}/disable"), json!({ "password": PASSWORD })).await;
    assert_eq!(disabled.status(), StatusCode::NO_CONTENT);

    let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    // The events are there, with the target's ID.
    assert!(text.contains("admin: account created") && text.contains("admin: password reset code issued"), "{text}");
    assert!(text.contains(&ana_id.to_string()), "the target's ID is logged");
    // Nothing secret is.
    let join_url = made.invite.join_url.clone().unwrap();
    let secrets = [
        PASSWORD,
        "a wrong password",
        new_password,
        made.invite.code.as_str(),
        reset.code.as_str(),
        join_url.as_str(),
        token.as_str(),
        new_token.as_str(),
    ];
    for secret in secrets {
        assert!(!text.contains(secret), "{secret} in the log");
    }
    let lower = text.to_ascii_lowercase();
    for code in [&made.invite.code, &reset.code] {
        assert!(!lower.contains(&code.to_ascii_lowercase()), "a code in the log");
    }
    // The usernames of targets are not logged at info or above (the sign-in's own debug line is the one place).
    assert!(!text.lines().any(|l| l.contains(" INFO ") && (l.contains("newbie") || l.contains("ana"))), "{text}");
}
