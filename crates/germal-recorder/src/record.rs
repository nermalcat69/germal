//! 启动一个有界面的 Chromium，捕获所有标签页的网络请求并写入 SQLite。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use chromiumoxide::Page;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::network::{
    EnableParams, EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent,
    EventRequestWillBeSentExtraInfo, EventResponseReceived, EventResponseReceivedExtraInfo,
    GetRequestPostDataParams, GetResponseBodyParams, ResourceType, Response,
};
use chromiumoxide::cdp::browser_protocol::page::{
    EnableParams as PageEnableParams, EventFrameNavigated, EventNavigatedWithinDocument,
    NavigateParams,
};
use futures::{FutureExt, StreamExt};

use crate::db::{Db, Record};

/// 超过此大小的响应体不入库（与原 TS 脚本一致：1 MB）。
const MAX_BODY: usize = 1_000_000;

type Shared = Arc<Mutex<Db>>;

/// 录到 `stop` 完成（或浏览器被关掉）为止，返回本次录到的条数。
pub async fn run(
    db_path: &Path,
    all: bool,
    start_url: Option<&str>,
    project: i64,
    stop: impl Future<Output = ()>,
) -> Result<i64> {
    let db = Db::open(db_path)?;
    let session = db.new_session()?;
    let db: Shared = Arc::new(Mutex::new(db));

    // 每次录制一个全新的临时 profile：固定的默认 profile 目录会被上次残留的 Chrome 进程锁住，
    // 之后的启动就悄悄失败。代价是不保留登录态。
    let profile = tempfile::tempdir()?;
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .with_head()
            // chromiumoxide 默认把页面钉成 800×600 的模拟视口，窗口再大右侧和下方也是黑的；None = 跟随真实窗口
            .viewport(None)
            .user_data_dir(profile.path())
            .build()
            .map_err(anyhow::Error::msg)?,
    )
    .await?;
    let driver = tokio::spawn(async move {
        while let Some(h) = handler.next().await {
            if h.is_err() {
                break;
            }
        }
    });
    // 第一个标签页：先挂好监听再导航，否则页面加载时最早的那批请求会漏掉
    let first = browser
        .new_page("about:blank")
        .await
        .context("open first tab")?;
    let mut seen = HashSet::new();
    seen.insert(first.target_id().clone());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(watch(
        first.clone(),
        db.clone(),
        session,
        project,
        all,
        Some(ready_tx),
    ));
    let _ = ready_rx.await;
    if let Some(url) = start_url {
        // 只发 Page.navigate，不等 load：goto 会等导航结束，遇到页面里 pushState 换页会一直等到超时
        first
            .execute(NavigateParams::new(url))
            .await
            .context("navigate to start URL")?;
    }

    // 用户可以随时开新标签 / 弹窗：轮询目标列表，给没接过的页面挂监听。
    // ponytail: 500 ms 轮询，新页面最初半秒的请求可能漏掉；要零遗漏就改订阅 Target.attachedToTarget。
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
        if driver.is_finished() {
            break;
        }
        let Ok(pages) = browser.pages().await else {
            break;
        };
        for page in pages {
            if seen.insert(page.target_id().clone()) {
                tokio::spawn(watch(page, db.clone(), session, project, all, None));
            }
        }
    }
    let _ = browser.close().await;
    let _ = driver.await;
    let n: i64 = db.lock().unwrap().0.query_row(
        "SELECT COUNT(*) FROM requests WHERE session_id = ?1",
        [session],
        |r| r.get(0),
    )?;
    Ok(n)
}

fn headers_json(h: &chromiumoxide::cdp::browser_protocol::network::Headers) -> String {
    h.inner().to_string()
}

fn finish_with_response(rec: &mut Record, res: &Response) {
    rec.status = Some(res.status as u16);
    rec.res_headers = Some(headers_json(&res.headers));
    rec.res_mime = Some(res.mime_type.clone());
}

/// 把 ExtraInfo 里的头并进已有的头（后者覆盖同名，大小写不敏感）。
/// 网络栈补上的 `Cookie`、有时还有 `Authorization`，只出现在 ExtraInfo 里；响应的 `Set-Cookie` 同理。
fn merge_headers(base: Option<&str>, extra: &serde_json::Value) -> String {
    let mut out: serde_json::Map<String, serde_json::Value> = base
        .and_then(|b| serde_json::from_str(b).ok())
        .unwrap_or_default();
    if let Some(extra) = extra.as_object() {
        for (k, v) in extra {
            out.retain(|existing, _| !existing.eq_ignore_ascii_case(k));
            out.insert(k.clone(), v.clone());
        }
    }
    serde_json::Value::Object(out).to_string()
}

fn ms(seconds: f64) -> f64 {
    seconds * 1000.0
}

struct Pending {
    rec: Record,
    /// CDP 单调时钟（秒），只用来算差值
    start: f64,
    /// 其余全部信息（协议、远端地址、TLS、分段耗时、发起者…），入库时序列化进 `meta` 列
    meta: serde_json::Map<String, serde_json::Value>,
    /// 重定向链：此前各跳的 URL
    chain: Vec<String>,
}

fn json_of<T: serde::Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or_default()
}

/// 去掉 null 与已单独存放的键
fn compact(v: serde_json::Value, drop: &[&str]) -> serde_json::Map<String, serde_json::Value> {
    let mut obj = match v {
        serde_json::Value::Object(o) => o,
        _ => Default::default(),
    };
    obj.retain(|k, v| !v.is_null() && !drop.contains(&k.as_str()));
    obj
}

/// 分段耗时（毫秒）：CDP 的 timing 各字段是相对 requestTime 的偏移，不适用的为 -1。
fn timing_breakdown(t: &serde_json::Value) -> serde_json::Value {
    let f = |k: &str| t.get(k).and_then(|v| v.as_f64()).filter(|v| *v >= 0.0);
    let span = |a: &str, b: &str| Some((f(b)? - f(a)?).max(0.0));
    let mut out = serde_json::Map::new();
    let mut put = |name: &str, v: Option<f64>| {
        if let Some(v) = v {
            out.insert(name.into(), serde_json::json!((v * 100.0).round() / 100.0));
        }
    };
    put("dns", span("dnsStart", "dnsEnd"));
    put("connect", span("connectStart", "connectEnd"));
    put("ssl", span("sslStart", "sslEnd"));
    put("send", span("sendStart", "sendEnd"));
    put("wait", span("sendEnd", "receiveHeadersStart"));
    serde_json::Value::Object(out)
}

/// 把一个响应记进 Pending：状态 / 头 / mime、完整的 Response 元数据，
/// 以及浏览器实际发出的请求头（比 requestWillBeSent 里的更全）。
fn apply_response(p: &mut Pending, res: &Response) {
    finish_with_response(&mut p.rec, res);
    let mut obj = compact(json_of(res), &["headers", "url", "status", "mimeType"]);
    if let Some(h) = obj.remove("requestHeaders") {
        p.rec.req_headers = merge_headers(Some(&p.rec.req_headers), &h);
    }
    if let Some(t) = obj.get("timing") {
        p.meta.insert("timingMs".into(), timing_breakdown(t));
    }
    p.meta
        .insert("response".into(), serde_json::Value::Object(obj));
}

async fn watch(
    page: Page,
    db: Shared,
    session: i64,
    project: i64,
    all: bool,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
) {
    if page.execute(EnableParams::default()).await.is_err() {
        return;
    }
    // Page 域给出标签页当前的真实地址：`documentURL` 只在整页加载时变，SPA 用 pushState 换页不会更新它
    let _ = page.execute(PageEnableParams::default()).await;
    let (
        Ok(mut req_ev),
        Ok(mut res_ev),
        Ok(mut done_ev),
        Ok(mut fail_ev),
        Ok(mut req_extra_ev),
        Ok(mut res_extra_ev),
        Ok(mut nav_ev),
        Ok(mut same_doc_ev),
    ) = (
        page.event_listener::<EventRequestWillBeSent>().await,
        page.event_listener::<EventResponseReceived>().await,
        page.event_listener::<EventLoadingFinished>().await,
        page.event_listener::<EventLoadingFailed>().await,
        page.event_listener::<EventRequestWillBeSentExtraInfo>()
            .await,
        page.event_listener::<EventResponseReceivedExtraInfo>()
            .await,
        page.event_listener::<EventFrameNavigated>().await,
        page.event_listener::<EventNavigatedWithinDocument>().await,
    )
    else {
        return;
    };
    let keep = |t: &Option<ResourceType>| {
        all || matches!(t, Some(ResourceType::Xhr | ResourceType::Fetch))
    };
    // 标签页顶层当前所在的页面：所有请求（含 iframe、广告脚本发的）都归到它名下
    let mut top_url: Option<String> = page.url().await.ok().flatten();
    if let Some(ready) = ready {
        let _ = ready.send(());
    }
    let mut main_frame = None;
    let mut pending: HashMap<String, Pending> = HashMap::new();
    // ExtraInfo 与主事件谁先到不定：先存着，入库时一并合并
    let mut extra_req: HashMap<String, (serde_json::Value, serde_json::Value)> = HashMap::new();
    let mut extra_res: HashMap<String, serde_json::Value> = HashMap::new();
    macro_rules! store {
        ($id:expr, $p:expr) => {{
            let mut p: Pending = $p;
            if let Some((headers, cookies)) = extra_req.remove($id) {
                p.rec.req_headers = merge_headers(Some(&p.rec.req_headers), &headers);
                if cookies.as_array().is_some_and(|c| !c.is_empty()) {
                    p.meta.insert("associatedCookies".into(), cookies);
                }
            }
            if let Some(x) = extra_res.remove($id) {
                p.rec.res_headers = Some(merge_headers(p.rec.res_headers.as_deref(), &x));
            }
            if !p.chain.is_empty() {
                p.meta
                    .insert("redirectedFrom".into(), serde_json::json!(p.chain));
            }
            p.rec.meta = Some(serde_json::Value::Object(std::mem::take(&mut p.meta)).to_string());
            if let Err(e) = db.lock().unwrap().insert(session, project, &p.rec) {
                tracing::warn!("db insert failed: {e}");
            }
        }};
    }
    // 换页事件与它之后发出的请求分属两条通道，select! 不保证先后：先把已到达的换页事件吃掉再处理请求，
    // 否则紧跟在 pushState 之后的 fetch 会被记到上一个页面名下。CDP 按序投递，请求已就绪则换页事件必然已在通道里。
    macro_rules! apply_nav {
        (frame $e:expr) => {{
            if $e.frame.parent_id.is_none() {
                main_frame = Some($e.frame.id.clone());
                top_url = Some($e.frame.url.clone());
            }
        }};
        (same_doc $e:expr) => {{
            if main_frame.as_ref().is_none_or(|f| *f == $e.frame_id) {
                top_url = Some($e.url.clone());
            }
        }};
    }
    loop {
        tokio::select! {
            Some(e) = req_ev.next() => {
                while let Some(Some(n)) = nav_ev.next().now_or_never() { apply_nav!(frame n); }
                while let Some(Some(n)) = same_doc_ev.next().now_or_never() { apply_nav!(same_doc n); }
                let id = e.request_id.inner().clone();
                let now = *e.timestamp.inner();
                // 重定向复用同一个 request_id：先把上一跳按其响应收尾
                let mut chain = Vec::new();
                if let (Some(mut prev), Some(res)) = (pending.remove(&id), e.redirect_response.as_ref()) {
                    apply_response(&mut prev, res);
                    prev.rec.duration_ms = Some(ms(now - prev.start));
                    chain = prev.chain.clone();
                    chain.push(prev.rec.url.clone());
                    store!(&id, prev);
                }
                if !keep(&e.r#type) { continue }
                let post = if e.request.has_post_data == Some(true) {
                    page.execute(GetRequestPostDataParams::new(e.request_id.clone()))
                        .await.ok().map(|r| r.result.post_data)
                } else { None };
                let mut meta = compact(json_of(&e.request), &["headers", "url", "urlFragment", "method", "hasPostData", "postDataEntries"]);
                meta.insert("documentUrl".into(), e.document_url.clone().into());
                meta.insert("initiator".into(), json_of(&e.initiator));
                for (k, v) in [("frameId", json_of(&e.frame_id)), ("hasUserGesture", json_of(&e.has_user_gesture)), ("loaderId", json_of(&e.loader_id))] {
                    if !v.is_null() { meta.insert(k.into(), v); }
                }
                pending.insert(id, Pending {
                    meta,
                    chain,
                    start: now,
                    rec: Record {
                        method: e.request.method.clone(),
                        url: e.request.url.clone(),
                        resource_type: e.r#type.as_ref().map(|t| t.as_ref().to_string()).unwrap_or_default(),
                        req_headers: headers_json(&e.request.headers),
                        post_data: post,
                        started_ms: Some(ms(*e.wall_time.inner()) as i64),
                        page_url: Some(top_url.clone().unwrap_or_else(|| e.document_url.clone())),
                        ..Default::default()
                    },
                });
            }
            Some(e) = nav_ev.next() => apply_nav!(frame e),
            Some(e) = same_doc_ev.next() => apply_nav!(same_doc e),
            Some(e) = req_extra_ev.next() => {
                extra_req.insert(e.request_id.inner().clone(), (e.headers.inner().clone(), json_of(&e.associated_cookies)));
            }
            Some(e) = res_extra_ev.next() => {
                extra_res.insert(e.request_id.inner().clone(), e.headers.inner().clone());
            }
            Some(e) = res_ev.next() => {
                if let Some(p) = pending.get_mut(e.request_id.inner()) {
                    apply_response(p, &e.response);
                    p.rec.ttfb_ms = Some(ms(*e.timestamp.inner() - p.start));
                }
            }
            Some(e) = done_ev.next() => {
                let id = e.request_id.inner().clone();
                if let Some(mut p) = pending.remove(&id) {
                    p.rec.duration_ms = Some(ms(*e.timestamp.inner() - p.start));
                    p.rec.res_size = Some(e.encoded_data_length as i64);
                    if let Ok(b) = page.execute(GetResponseBodyParams::new(e.request_id.clone())).await {
                        if b.result.body.len() > MAX_BODY {
                            p.rec.res_body = Some(format!("<{} bytes omitted>", b.result.body.len()));
                        } else {
                            p.rec.res_body = Some(b.result.body.clone());
                            p.rec.res_body_b64 = b.result.base64_encoded;
                        }
                    }
                    store!(&id, p);
                }
            }
            Some(e) = fail_ev.next() => {
                let id = e.request_id.inner().clone();
                if let Some(mut p) = pending.remove(&id) {
                    p.rec.duration_ms = Some(ms(*e.timestamp.inner() - p.start));
                    p.rec.error = Some(e.error_text.clone());
                    store!(&id, p);
                }
            }
            else => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::merge_headers;

    #[test]
    fn extra_info_adds_cookie_and_overrides_case_insensitively() {
        let merged = merge_headers(
            Some(r#"{"accept":"*/*","Authorization":"old"}"#),
            &serde_json::json!({"Cookie":"a=1","authorization":"Bearer t"}),
        );
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["Cookie"], "a=1");
        assert_eq!(v["authorization"], "Bearer t");
        assert!(v.get("Authorization").is_none());
        assert_eq!(v["accept"], "*/*");
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    #[test]
    fn timing_breakdown_skips_inapplicable_phases() {
        let t = serde_json::json!({
            "dnsStart": -1, "dnsEnd": -1, "connectStart": 2.0, "connectEnd": 12.5,
            "sslStart": 5.0, "sslEnd": 12.5, "sendStart": 12.6, "sendEnd": 12.9,
            "receiveHeadersStart": 40.9
        });
        let b = timing_breakdown(&t);
        assert!(b.get("dns").is_none());
        assert_eq!(b["connect"], 10.5);
        assert_eq!(b["ssl"], 7.5);
        assert_eq!(b["wait"], 28.0);
    }

    #[test]
    fn compact_drops_nulls_and_listed_keys() {
        let o = compact(
            serde_json::json!({"a":1,"b":null,"headers":{}}),
            &["headers"],
        );
        assert_eq!(o.len(), 1);
    }
}
