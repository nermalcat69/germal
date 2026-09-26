//! 端到端：真开一个 Chromium（有界面），页面里用 pushState 换页再发请求，
//! 录到的 `page_url` 必须跟着换。需要本机装有 Chrome / Chromium，默认忽略：
//!
//!   cargo test -p germal-recorder --test spa_pages -- --ignored --nocapture

use std::time::Duration;

use germal_recorder::{db::Db, record};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PAGE: &str = r#"<!doctype html><title>spa</title><script>
(async () => {
  await fetch('/api/home');
  history.pushState({}, '', '/login');
  await fetch('/api/login');
  history.pushState({}, '', '/pricing?plan=pro');
  await fetch('/api/pricing');
})();
</script>"#;

async fn serve(listener: TcpListener) {
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let path = head.split_whitespace().nth(1).unwrap_or("/");
            let (ctype, body) = if path.starts_with("/api/") {
                ("application/json", r#"{"ok":true}"#)
            } else {
                ("text/html", PAGE)
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}

#[tokio::test]
#[ignore = "launches a real Chromium window"]
async fn requests_follow_pushstate_navigation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(serve(listener));

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("rec.db");
    record::run(&db_path, false, Some(&format!("{base}/")), 1, async {
        tokio::time::sleep(Duration::from_secs(6)).await;
    })
    .await
    .unwrap_or_else(|e| panic!("{e:#}"));

    let db = Db::open(&db_path).unwrap();
    let pages: std::collections::HashMap<String, Option<String>> = db
        .list_after(1, 0, 100)
        .unwrap()
        .into_iter()
        .map(|e| (e.path, e.page_url))
        .collect();
    let want = |path: &str, page: &str| {
        assert_eq!(
            pages.get(path).cloned().flatten().as_deref(),
            Some(format!("{base}{page}").as_str()),
            "{pages:?}"
        )
    };
    want("/api/home", "/");
    want("/api/login", "/login");
    want("/api/pricing", "/pricing?plan=pro");
}
