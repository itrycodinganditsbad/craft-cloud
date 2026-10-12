//! Proxy settings in the environment never carry the agent's plain-HTTP loopback traffic (token, password).
//!
//! The environment variable is set only for a child process (this test binary, run again), so no other test sees it.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use craft_cloud_agent::http::{ServerUrl, info};

const CHILD: &str = "CC_PROXY_TEST_CHILD";

fn read_request(stream: &mut std::net::TcpStream) {
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf);
}

#[test]
fn proxy_settings_are_ignored() {
    if std::env::var_os(CHILD).is_some() {
        // Child: a direct server on loopback; the request must reach it, not the proxy named in the environment.
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&accepted);
        // Not joined: if the request goes elsewhere this thread waits forever, and the process ending ends it.
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = server.accept() {
                seen.store(true, Ordering::SeqCst);
                read_request(&mut stream);
                let body = "{}";
                let _ = write!(stream, "HTTP/1.1 500 x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        let url = ServerUrl::parse(&format!("http://127.0.0.1:{port}")).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _ = rt.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(5), info(&url)).await });
        assert!(accepted.load(Ordering::SeqCst), "the request never reached the server");
        return;
    }

    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "proxy_settings_are_ignored", "--test-threads=1"])
        .env(CHILD, "1")
        .env("HTTP_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .status()
        .unwrap();
    assert!(status.success(), "the request must go straight to the server (the child's server saw nothing)");
    assert!(proxy.accept().is_err(), "the proxy was contacted");
}
