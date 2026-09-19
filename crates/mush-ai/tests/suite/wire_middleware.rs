use mush_ai::http;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn middleware_journals_request_and_response() {
    let dir = std::env::temp_dir().join(format!("mush-wire-mw-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("wire.jsonl");
    // SAFETY: single-threaded test setup before any client touches the env
    unsafe {
        std::env::set_var("MUSH_WIRE_LOG", &path);
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let _ = sock.read(&mut buf).await.unwrap();
        let body = r#"{"ok":true,"secret":"sk-1234567890ab"}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
        sock.flush().await.unwrap();
    });

    let client = http::client();
    let url = format!("http://{addr}/v1/models");
    let resp = client.post(&url).body("hello-body").send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("\"ok\":true"),
        "body passthrough broken: {text}"
    );
    server.await.unwrap();

    let recs: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let req = recs
        .iter()
        .find(|v| v["kind"] == "request")
        .expect("no request record");
    let res = recs
        .iter()
        .find(|v| v["kind"] == "response")
        .expect("no response record");

    assert_eq!(req["method"], "POST");
    assert!(req["url"].as_str().unwrap().contains("/v1/models"));
    assert_eq!(req["body"], "hello-body");

    assert_eq!(res["status"], 200);
    assert_eq!(res["body"]["ok"], true);
    assert_eq!(res["body"]["secret"], "<redacted>");

    assert_eq!(req["call_id"], res["call_id"]);

    std::fs::remove_dir_all(&dir).ok();
}
