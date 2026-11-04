//! What the admin pages write to the log: events with the kind and the target's ID, and no password, session cookie, CSRF
//! token, challenge, code, recovery code, invite or reset code, or join link. The subscriber is global, so this is the only
//! test in its binary.

use std::sync::{Arc, Mutex};

use craft_cloud_meta::{Meta, Role};
use craft_cloud_proto::api::{LoginResponse, RecoveryCodes, TwoFactorSetupResponse};
use craft_cloud_server::AppState;
use craft_cloud_server::auth::hash_password;
use craft_cloud_server::secretbox::{SecretBox, TwoFactorKey};
use craft_cloud_server::totp;
use craft_cloud_server::web::{COOKIE_NAME, WebSettings};
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
const ORIGIN: &str = "https://cloud.example.org";

fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| {
            let esc: String = v
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') { char::from(b).to_string() } else { format!("%{b:02X}") }
                })
                .collect();
            format!("{k}={esc}")
        })
        .collect::<Vec<_>>()
        .join("&")
}

async fn post_form(client: &Client, base: &str, path: &str, token: Option<&str>, fields: Vec<(&'static str, String)>) -> reqwest::Response {
    let borrowed: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut req = client
        .post(format!("{base}{path}"))
        .header("origin", ORIGIN)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&borrowed));
    if let Some(token) = token {
        req = req.header("cookie", format!("{COOKIE_NAME}={token}"));
    }
    req.send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_admin_pages_log_no_secret() {
    let log = Capture::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt().with_writer(log.clone()).with_max_level(tracing::Level::DEBUG).with_ansi(false).finish(),
    )
    .unwrap();

    let db = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let meta = Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    let store = Store::new(DiskBackend::new(blobs.path()).unwrap());
    let state = AppState::new(meta.clone(), store, vec![])
        .unwrap()
        .with_public_url(Some(ORIGIN.to_owned()))
        .with_web_admin(Some(WebSettings::new(ORIGIN, 30 * 60_000, 4 * 3_600_000).unwrap()))
        .with_two_factor(Some(SecretBox::new(&TwoFactorKey::from_bytes([42; 32]), None).unwrap()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(craft_cloud_server::serve(state, listener, std::future::pending::<()>()));
    let client = Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    meta.create_user("root", &hash_password(PASSWORD).unwrap(), Role::Admin, 0).await.unwrap();
    meta.create_user("ana", &hash_password(PASSWORD).unwrap(), Role::Member, 0).await.unwrap();
    let ana_id = meta.user_by_name("ana").await.unwrap().unwrap().id.0;

    // Two-factor sign-in for root, from a device; its token and codes are secrets too.
    let device = client
        .post(format!("{base}/api/v1/auth/login"))
        .json(&json!({ "username": "root", "password": PASSWORD, "device_name": "pc" }))
        .send()
        .await
        .unwrap()
        .json::<LoginResponse>()
        .await
        .unwrap()
        .token;
    let setup: TwoFactorSetupResponse = client
        .post(format!("{base}/api/v1/account/two-factor/setup"))
        .bearer_auth(&device)
        .json(&json!({ "password": PASSWORD }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret = totp::base32_decode(&setup.secret).unwrap();
    let first_code = totp::code_for(&secret, totp::step_at(craft_cloud_server::state::now_ms())).unwrap();
    let recovery: RecoveryCodes = client
        .post(format!("{base}/api/v1/account/two-factor/confirm"))
        .bearer_auth(&device)
        .json(&json!({ "totp_code": first_code }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let recovery_code = recovery.recovery_codes.first().unwrap().clone();

    // A wrong password, the password step with a challenge, a wrong code, a recovery code that signs in.
    let wrong = post_form(&client, &base, "/web/login", None, vec![("username", "root".into()), ("password", "a wrong password".into())]).await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    let step =
        post_form(&client, &base, "/web/login", None, vec![("username", "root".into()), ("password", PASSWORD.into())]).await.text().await.unwrap();
    let marker = "name=\"challenge\" value=\"";
    let challenge = step[step.find(marker).unwrap() + marker.len()..].split('"').next().unwrap().to_owned();
    let bad =
        post_form(&client, &base, "/web/login/second-factor", None, vec![("challenge", challenge.clone()), ("totp_code", "000000".into())]).await;
    assert_eq!(bad.status(), StatusCode::FORBIDDEN);
    let signed =
        post_form(&client, &base, "/web/login/second-factor", None, vec![("challenge", challenge.clone()), ("recovery_code", recovery_code.clone())])
            .await;
    assert_eq!(signed.status(), StatusCode::SEE_OTHER);
    let line = signed.headers().get("set-cookie").unwrap().to_str().unwrap().to_owned();
    let token = line.strip_prefix(&format!("{COOKIE_NAME}=")).unwrap().split(';').next().unwrap().to_owned();
    let activity: serde_json::Value =
        client.get(format!("{base}/api/v1/account/activity?limit=100")).bearer_auth(&device).send().await.unwrap().json().await.unwrap();
    assert!(activity["events"].as_array().unwrap().iter().any(|event| event["action"] == "auth.login" && event["details"]["client_kind"] == "web"));
    let audit: serde_json::Value =
        client.get(format!("{base}/api/v1/admin/audit?limit=200")).bearer_auth(&device).send().await.unwrap().json().await.unwrap();
    let events = audit["events"].as_array().unwrap();
    assert!(events.iter().any(|event| event["action"] == "auth.login" && event["details"]["client_kind"] == "web"));
    assert!(events.iter().any(|event| event["action"] == "auth.login_failed" && event["details"]["client_kind"] == "web"));
    assert!(events.iter().any(|event| event["action"] == "auth.second_factor_failed" && event["details"]["client_kind"] == "web"));
    assert!(events.iter().any(|event| event["action"] == "auth.recovery_code_used" && event["details"]["client_kind"] == "web"));
    let export = client.get(format!("{base}/api/v1/admin/audit/export")).bearer_auth(&device).send().await.unwrap().text().await.unwrap();
    assert!(export.lines().any(|line| line.contains("\"action\":\"auth.login\"") && line.contains("\"client_kind\":\"web\"")), "{export}");
    assert!(export.lines().any(|line| line.contains("\"action\":\"auth.login_failed\"") && line.contains("\"client_kind\":\"web\"")), "{export}");
    assert!(
        export.lines().any(|line| line.contains("\"action\":\"auth.second_factor_failed\"") && line.contains("\"client_kind\":\"web\"")),
        "{export}"
    );
    assert!(
        export.lines().any(|line| line.contains("\"action\":\"auth.recovery_code_used\"") && line.contains("\"client_kind\":\"web\"")),
        "{export}"
    );
    let audit_page =
        client.get(format!("{base}/web/audit")).header("cookie", format!("{COOKIE_NAME}={token}")).send().await.unwrap().text().await.unwrap();
    assert!(audit_page.contains("&quot;client_kind&quot;:&quot;web&quot;"), "{audit_page}");
    let page = client.get(format!("{base}/web")).header("cookie", format!("{COOKIE_NAME}={token}")).send().await.unwrap().text().await.unwrap();
    let marker = "name=\"csrf\" value=\"";
    let csrf = page[page.find(marker).unwrap() + marker.len()..].split('"').next().unwrap().to_owned();

    // Changes: a new account with its invite, a reset code, a wrong confirmation, a disable, and the sign-out.
    let code_of = |html: &str| html.split("<span class=\"secret\">").nth(1).unwrap().split("</span>").next().unwrap().to_owned();
    // Every change is confirmed with a recovery code of its own (each works once).
    let mut spare = recovery.recovery_codes.iter().skip(1).cloned();
    let mut fields = |extra: Vec<(&'static str, String)>| {
        let mut all = extra;
        all.push(("csrf", csrf.clone()));
        all.push(("password", PASSWORD.to_owned()));
        all.push(("recovery_code", spare.next().unwrap()));
        all
    };
    let created = post_form(&client, &base, "/web/users", Some(&token), fields(vec![("username", "newbie".into()), ("role", "member".into())])).await;
    assert_eq!(created.status(), StatusCode::OK);
    let invite = code_of(&created.text().await.unwrap());
    let reset = post_form(&client, &base, &format!("/web/users/{ana_id}/password-reset"), Some(&token), fields(vec![])).await;
    assert_eq!(reset.status(), StatusCode::OK);
    let reset_code = code_of(&reset.text().await.unwrap());
    let refused = post_form(
        &client,
        &base,
        &format!("/web/users/{ana_id}/disable"),
        Some(&token),
        vec![("csrf", csrf.clone()), ("password", "another wrong password".into()), ("totp_code", "000000".into())],
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let disabled = post_form(&client, &base, &format!("/web/users/{ana_id}/disable"), Some(&token), fields(vec![])).await;
    assert_eq!(disabled.status(), StatusCode::SEE_OTHER);
    let out = post_form(&client, &base, "/web/logout", Some(&token), vec![("csrf", csrf.clone())]).await;
    assert_eq!(out.status(), StatusCode::SEE_OTHER);

    let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    // The events are there, with the target's ID.
    assert!(
        text.contains("web admin signed in") && text.contains("admin (web): account created") && text.contains("admin (web): account disabled"),
        "{text}"
    );
    assert!(text.contains(&ana_id.to_string()), "the target's ID is logged");
    // Nothing secret is.
    let join_url = format!("{ORIGIN}/join/{invite}");
    let all_codes = recovery.recovery_codes.clone();
    let secrets = [
        PASSWORD,
        "a wrong password",
        "another wrong password",
        token.as_str(),
        csrf.as_str(),
        challenge.as_str(),
        invite.as_str(),
        reset_code.as_str(),
        recovery_code.as_str(),
        device.as_str(),
        setup.secret.as_str(),
        first_code.as_str(),
    ];
    for secret in secrets {
        assert!(!text.contains(secret), "{secret} in the log");
    }
    for code in &all_codes {
        assert!(
            !text.contains(code.as_str()) && !text.to_ascii_lowercase().contains(&code.replace('-', "").to_ascii_lowercase()),
            "a recovery code in the log"
        );
    }
    let lower = text.to_ascii_lowercase();
    for code in [&invite, &reset_code, &recovery_code.replace('-', "")] {
        assert!(!lower.contains(&code.to_ascii_lowercase()), "a code in the log");
    }
    assert!(!text.contains(&join_url) && !text.contains("/join/"));
    // The usernames are not logged at info or above.
    assert!(!text.lines().any(|l| l.contains(" INFO ") && (l.contains("newbie") || l.contains("root") || l.contains("ana"))), "{text}");
}
