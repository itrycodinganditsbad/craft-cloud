//! An agent from before `/v1/blobs` (as it serves: axum, a 404 fallback that doesn't read the body, the 1 MiB body
//! limit), and the app client putting blobs of every size: always `agent_too_old`, never a broken connection.
//! (The review's probe, kept as a test.)

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use craft_cloud_proto::local::{LOCAL_API_FILE, LOCAL_API_MIN, LocalApiFile, RunState, Status};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_old_agent_is_too_old_for_blobs_of_any_size() {
    // (An agent from before blobs speaks the first local API version.)
    let status = Status {
        api: LOCAL_API_MIN,
        signed_in: true,
        account: Some("a".into()),
        server: Some("https://x.invalid".into()),
        folder: None,
        machine: Some("m".into()),
        online: true,
        run_state: RunState::Idle,
        autostart: None,
        last_sync_ms: None,
        removed: false,
    };
    let app = Router::new()
        .route("/v1/status", get(move || async move { axum::Json(status.clone()) }))
        .fallback(|| async {
            (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({ "error": "not_found", "message": "no such call" }))).into_response()
        })
        .layer(DefaultBodyLimit::max(1024 * 1024));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let file = LocalApiFile { schema: craft_cloud_proto::local::LOCAL_API_SCHEMA, port, token: "t".into(), pid: 1 };
    std::fs::write(dir.path().join(LOCAL_API_FILE), serde_json::to_vec(&file).unwrap()).unwrap();
    let path = dir.path().to_owned();
    let results = tokio::task::spawn_blocking(move || {
        let client = craft_cloud_app::Client::connect_in(&path).unwrap();
        let mut out = Vec::new();
        for len in [10usize, 512 * 1024, 5 * 1024 * 1024, 32 * 1024 * 1024] {
            // A fresh client each time: the first question isn't remembered from the last.
            let fresh = craft_cloud_app::Client::connect_in(&path).unwrap();
            out.push((len, fresh.blobs().put(&vec![7u8; len])));
        }
        out.push((0, client.blobs().get(&[1; 32], 3).map(|_| craft_cloud_app::blobs::Stored { hash: [0; 32], len: 0 })));
        out
    })
    .await
    .unwrap();
    for (len, r) in results {
        assert!(matches!(&r, Err(craft_cloud_app::Error::Refused { code, .. }) if code == "agent_too_old"), "{len} bytes: {r:?}");
    }
}
