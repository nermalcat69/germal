//! 把录下的请求装载回 Germal：转成 `RequestDraft`，导入数据目录或直接压测。

use germal_core::model::{BodyKind, KeyValue, Method, RawFormat, RequestDraft, SavedRequest};
use rusqlite::{Row, params_from_iter};

use crate::db::Db;

pub struct Row_ {
    pub id: i64,
    pub method: String,
    pub url: String,
    pub host: String,
    pub status: Option<u16>,
    pub req_headers: String,
    pub post_data: Option<String>,
    pub imported: bool,
}

/// `category`：GET / POST / OTHER；`session`：只取某次录制；`host`：只取某个域名。
pub fn select(
    db: &Db,
    category: Option<&str>,
    session: Option<i64>,
    host: Option<&str>,
) -> rusqlite::Result<Vec<Row_>> {
    let mut sql =
        "SELECT id, method, url, host, status, req_headers, post_data, imported FROM requests WHERE 1=1"
            .to_string();
    let mut args: Vec<String> = Vec::new();
    if let Some(c) = category {
        args.push(c.to_ascii_uppercase());
        sql += &format!(" AND category = ?{}", args.len());
    }
    if let Some(s) = session {
        args.push(s.to_string());
        sql += &format!(" AND session_id = CAST(?{} AS INTEGER)", args.len());
    }
    if let Some(h) = host {
        args.push(h.to_string());
        sql += &format!(" AND host = ?{}", args.len());
    }
    sql += " ORDER BY id";
    let map = |r: &Row| {
        Ok(Row_ {
            id: r.get(0)?,
            method: r.get(1)?,
            url: r.get(2)?,
            host: r.get(3)?,
            status: r.get(4)?,
            req_headers: r.get(5)?,
            post_data: r.get(6)?,
            imported: r.get(7)?,
        })
    };
    db.0.prepare(&sql)?
        .query_map(params_from_iter(args), map)?
        .collect()
}

/// 把还没导入过的录制转成已保存请求并标记为已导入，重复导入不会产生重复项。
/// 返回 (成功数据, 因方法不支持而跳过的条数)。
pub fn take_new(
    db: &Db,
    category: Option<&str>,
    session: Option<i64>,
    host: Option<&str>,
) -> rusqlite::Result<(Vec<SavedRequest>, usize)> {
    let (mut out, mut skipped) = (Vec::new(), 0);
    for r in select(db, category, session, host)?
        .iter()
        .filter(|r| !r.imported)
    {
        match to_saved(r) {
            Some(s) => out.push(s),
            None => skipped += 1,
        }
        db.0.execute("UPDATE requests SET imported = 1 WHERE id = ?1", [r.id])?;
    }
    Ok((out, skipped))
}

/// 方法 Germal 不支持（TRACE、CONNECT…）返回 None。
pub fn to_draft(r: &Row_) -> Option<RequestDraft> {
    let method = Method::parse(&r.method)?;
    let mut headers = Vec::new();
    let mut content_type = String::new();
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&r.req_headers) {
        for (k, v) in map {
            let lk = k.to_ascii_lowercase();
            // 伪头由协议层生成；这三个由 HTTP 客户端按实际连接与 body 重算
            if k.starts_with(':')
                || matches!(lk.as_str(), "host" | "content-length" | "transfer-encoding")
            {
                continue;
            }
            let v = v
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string());
            if lk == "content-type" {
                content_type = v.to_ascii_lowercase();
            }
            headers.push(KeyValue::new(k, v));
        }
    }
    let body = match &r.post_data {
        None => BodyKind::None,
        Some(text) => BodyKind::Raw {
            format: if content_type.contains("json") {
                RawFormat::Json
            } else if content_type.contains("xml") {
                RawFormat::Xml
            } else {
                RawFormat::Text
            },
            text: text.clone(),
        },
    };
    Some(RequestDraft {
        method,
        url: r.url.clone(),
        headers,
        body,
        ..Default::default()
    })
}

/// 存成侧栏里的一条已保存请求，分类 = 域名。
pub fn to_saved(r: &Row_) -> Option<SavedRequest> {
    let draft = to_draft(r)?;
    let path = url::Url::parse(&r.url)
        .ok()
        .map(|u| u.path().to_string())
        .unwrap_or_default();
    let mut saved = SavedRequest::new(format!("{} {}", r.method, path), draft);
    saved.group = Some(r.host.clone());
    Some(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_json_becomes_raw_json_and_drops_computed_headers() {
        let r = Row_ {
            id: 1,
            method: "POST".into(),
            url: "https://a.test/api".into(),
            host: "a.test".into(),
            status: Some(200),
            req_headers:
                r#"{"Content-Type":"application/json","Content-Length":"2",":path":"/api"}"#.into(),
            post_data: Some("{}".into()),
            imported: false,
        };
        let d = to_draft(&r).unwrap();
        assert_eq!(d.headers.len(), 1);
        assert!(matches!(
            d.body,
            BodyKind::Raw {
                format: RawFormat::Json,
                ..
            }
        ));
        assert!(
            to_draft(&Row_ {
                method: "TRACE".into(),
                ..r
            })
            .is_none()
        );
    }
}
