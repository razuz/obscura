// Regression for issue #977: in stealth mode, scripted fetch()/XHR is routed
// through the wreq transport, which returned before reaching the recorder in
// op_fetch_url. The request executed normally and was invisible to every CDP
// client -- no Network.requestWillBeSent, no responseReceived, and
// Network.getResponseBody could not resolve it.
//
// This is the stealth twin of js_fetch_emits_network_events.rs. The only
// difference that matters is CdpContext::new_with_options(None, true): the same
// page, the same script, the other transport. A scanner cares because an XHR
// POST to an exfil collector is exactly this shape -- under the bug it came
// back as "the page made no request" rather than as an error.

#![cfg(feature = "stealth")]

use obscura_cdp::dispatch::{dispatch, CdpContext};
use obscura_cdp::types::CdpRequest;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..6 {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..]);
                let (ct, body) = if req.starts_with("GET /api/data.json") {
                    ("application/json", "{\"value\":42}")
                } else {
                    (
                        "text/html",
                        r#"<html><head></head><body>
<div id="r">stage1</div>
<script>
window.__done = new Promise(function (resolve) {
  fetch("/api/data.json")
    .then(function (r) { return r.json(); })
    .then(function (d) { document.getElementById("r").textContent = "got:" + d.value; resolve("ok"); })
    .catch(function (e) { resolve("err:" + e); });
});
</script>
</body></html>"#,
                    )
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/")
}

async fn cdp(ctx: &mut CdpContext, id: u64, method: &str, params: Value, session_id: &str) -> Value {
    let resp = dispatch(
        &CdpRequest {
            id,
            method: method.to_string(),
            params,
            session_id: Some(session_id.to_string()),
        },
        ctx,
    )
    .await;
    assert!(resp.error.is_none(), "CDP {method} failed: {:?}", resp.error);
    resp.result.unwrap_or_else(|| json!({}))
}

fn response_request_id(ctx: &CdpContext, url_needle: &str) -> Option<String> {
    ctx.pending_events
        .iter()
        .find(|e| {
            e.method == "Network.responseReceived"
                && e.params
                    .get("response")
                    .and_then(|r| r.get("url"))
                    .and_then(|u| u.as_str())
                    .map(|u| u.contains(url_needle))
                    .unwrap_or(false)
        })
        .and_then(|e| e.params.get("requestId").and_then(|v| v.as_str()).map(str::to_string))
}

#[tokio::test(flavor = "current_thread")]
async fn stealth_js_fetch_emits_network_request_and_response() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let base = serve().await;
    // The one line that differs from the non-stealth twin.
    let mut ctx = CdpContext::new_with_options(None, true);
    let page_id = ctx.create_page();
    let session_id = "session-1";
    ctx.sessions.insert(session_id.to_string(), page_id.clone());

    cdp(
        &mut ctx,
        1,
        "Page.navigate",
        json!({"url": base, "waitUntil": "networkidle0"}),
        session_id,
    )
    .await;

    let request_urls = ctx
        .pending_events
        .iter()
        .filter(|e| e.method == "Network.requestWillBeSent")
        .filter_map(|e| {
            e.params
                .get("request")
                .and_then(|r| r.get("url"))
                .and_then(|u| u.as_str())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    assert!(
        request_urls.iter().any(|u| u.contains("/api/data.json")),
        "a script fetch under --stealth must emit Network.requestWillBeSent; saw {request_urls:?}"
    );

    let request_id = response_request_id(&ctx, "/api/data.json")
        .expect("stealth fetch must emit Network.responseReceived with a requestId");
    let body = cdp(
        &mut ctx,
        2,
        "Network.getResponseBody",
        json!({"requestId": request_id}),
        session_id,
    )
    .await;
    assert_eq!(
        body.get("body").and_then(|b| b.as_str()),
        Some("{\"value\":42}"),
        "Network.getResponseBody must resolve the stealth-fetched JSON by the same requestId"
    );
}
