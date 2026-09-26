//! 录制详情用到的纯函数：头解析、认证头识别、正文与时间格式化。与界面无关，单独放一个文件便于测试。

/// 正文最多渲染这么多字节，防止一个大响应拖垮布局。
pub const MAX_BODY_SHOWN: usize = 100_000;

pub fn parse_headers(json: &str) -> Vec<(String, String)> {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Object(map)) => map
            .into_iter()
            .map(|(k, v)| {
                let v = v
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string());
                (k, v)
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn header(h: &[(String, String)], name: &str) -> Option<String> {
    h.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

pub fn is_auth_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "api-key"
            | "x-auth-token"
            | "x-access-token"
            | "x-csrf-token"
            | "x-xsrf-token"
    )
}

/// 正文展示：JSON 美化；二进制（base64）只给个说明；超长截断。
pub fn body_text(body: &str, base64: bool, content_type: &str) -> String {
    if base64 {
        return format!("(binary, {} bytes base64)", body.len());
    }
    let looks_json = content_type.contains("json") || body.trim_start().starts_with(['{', '[']);
    let mut text = if looks_json && serde_json::from_str::<serde_json::Value>(body).is_ok() {
        String::from_utf8(germal_core::body::pretty::pretty_json(body.as_bytes()))
            .unwrap_or_else(|_| body.to_string())
    } else {
        body.to_string()
    };
    if text.len() > MAX_BODY_SHOWN {
        let mut end = MAX_BODY_SHOWN;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n… (truncated)");
    }
    text
}

pub fn fmt_ms(ms: f64) -> String {
    if ms >= 1000. {
        format!("{:.2} s", ms / 1000.)
    } else {
        format!("{ms:.0} ms")
    }
}

pub fn fmt_bytes(n: i64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / (1 << 10) as f64),
        n => format!("{n} B"),
    }
}

/// Unix 毫秒 → `YYYY-MM-DD HH:MM:SS.mmm UTC`（Hinnant 的 civil-from-days，免得为一个时间戳引入日期库）。
pub fn fmt_utc(ms: i64) -> String {
    let (secs, milli) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, tod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{milli:03} UTC",
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    )
}

use std::time::Duration;

use germal_core::loadtest::LoadReport;

pub fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.)
}

/// 可复制的纯文本汇总（「复制结果」按钮用）。
pub fn summary_text(target: &str, r: &LoadReport, host_rows: &[(String, String)]) -> String {
    let mut out = format!(
        "{target}\nsent {}/{} · completed {} · failed {} · transport errors {} · in flight {}\nelapsed {} · {:.1} req/s\nlatency min {} · p50 {} · p90 {} · p99 {} · max {}\nstatus: {}\n",
        r.sent,
        r.planned,
        r.completed,
        r.failed(),
        r.errors,
        r.in_flight,
        fmt_duration(r.elapsed),
        r.rps,
        ms(r.min),
        ms(r.p50),
        ms(r.p90),
        ms(r.p99),
        ms(r.max),
        r.statuses
            .iter()
            .map(|(s, n)| format!("{s}×{n}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    for (k, n) in &r.error_kinds {
        out.push_str(&format!("error: {k} ×{n}\n"));
    }
    for (k, v) in host_rows {
        out.push_str(&format!("{k}: {v}\n"));
    }
    out
}

/// 主机名 → 注册域（`api.shop.example.co.uk` → `example.co.uk`）。IP 与无点主机名原样返回。
/// ponytail: 没有内置公共后缀表，只识别 `co.uk` / `com.au` 这类「二级标签 + 两字母国家码」；
/// 遇到冷门后缀（如 `github.io` 当作站点）分错时换成 `psl` crate。
pub fn registrable_domain(host: &str) -> String {
    let host = host.to_ascii_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() || !host.contains('.') {
        return host;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let n = labels.len();
    const SLD: [&str; 10] = [
        "co", "com", "org", "net", "gov", "edu", "ac", "or", "ne", "go",
    ];
    let keep = if n >= 3 && labels[n - 1].len() == 2 && SLD.contains(&labels[n - 2]) {
        3
    } else {
        2
    };
    labels[n.saturating_sub(keep)..].join(".")
}

/// 压测耗时：不足 1 分钟带一位小数的秒（`0.4 s`），之后按量级 `2m 05s` / `1h 02m 05s` / `1d 02h 03m 04s`。
pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{:.1} s", d.as_secs_f64());
    }
    let (days, hours, mins, s) = (
        secs / 86_400,
        secs % 86_400 / 3600,
        secs % 3600 / 60,
        secs % 60,
    );
    match (days, hours) {
        (0, 0) => format!("{mins}m {s:02}s"),
        (0, _) => format!("{hours}h {mins:02}m {s:02}s"),
        _ => format!("{days}d {hours:02}h {mins:02}m {s:02}s"),
    }
}

/// 已运行时长 `HH:MM:SS`。
pub fn fmt_elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

/// 主机信息 → 展示行。服务商与 DNS 服务商放最前，其后每个 IP 一行（IPv4 / IPv6 分开标注）。
pub fn host_rows(info: &germal_core::hostinfo::HostInfo) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let mut providers: Vec<String> = info
        .ipv4
        .iter()
        .chain(&info.ipv6)
        .filter_map(|i| i.provider.clone())
        .collect();
    providers.sort();
    providers.dedup();
    if !providers.is_empty() {
        rows.push(("Provider".to_string(), providers.join("; ")));
    }
    if !info.nameservers.is_empty() {
        rows.push(("DNS servers".to_string(), info.nameservers.join(", ")));
    }
    for (label, list) in [("IPv4", &info.ipv4), ("IPv6", &info.ipv6)] {
        for i in list {
            let mut parts = vec![i.ip.map(|ip| ip.to_string()).unwrap_or_default()];
            if let Some(asn) = i.asn {
                parts.push(format!("AS{asn}"));
            }
            if let Some(p) = &i.provider {
                parts.push(p.clone());
            }
            if let Some(p) = &i.ptr {
                parts.push(format!("PTR {p}"));
            }
            if let Some(p) = &i.prefix {
                parts.push(p.clone());
            }
            if let Some(c) = &i.country {
                parts.push(c.clone());
            }
            rows.push((label.to_string(), parts.join(" · ")));
        }
    }
    if let Some(e) = &info.error {
        rows.push(("Error".to_string(), e.clone()));
    }
    rows
}

/// 把 `meta` JSON 摊平成 `a.b.c → 值` 的行，界面按行展示「其余全部信息」而不必逐字段写死。
/// 空串 / null / 空容器略过；标量数组合成一行。
pub fn flatten_meta(json: &str) -> Vec<(String, String)> {
    fn walk(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, String)>) {
        use serde_json::Value::*;
        match v {
            Null => {}
            String(s) if s.is_empty() => {}
            String(s) => out.push((prefix.to_string(), s.clone())),
            Bool(b) => out.push((prefix.to_string(), b.to_string())),
            Number(n) => out.push((prefix.to_string(), n.to_string())),
            Array(a) if a.iter().all(|x| !x.is_object() && !x.is_array()) => {
                let joined: Vec<_> = a
                    .iter()
                    .filter(|x| !x.is_null())
                    .map(|x| {
                        x.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| x.to_string())
                    })
                    .collect();
                if !joined.is_empty() {
                    out.push((prefix.to_string(), joined.join(", ")));
                }
            }
            Array(a) => {
                for (i, x) in a.iter().enumerate() {
                    walk(&format!("{prefix}[{i}]"), x, out);
                }
            }
            Object(o) => {
                for (k, x) in o {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(&key, x, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_str(json) {
        walk("", &v, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(fmt_utc(0), "1970-01-01 00:00:00.000 UTC");
        assert_eq!(fmt_utc(1_782_452_730_123), "2026-06-26 05:45:30.123 UTC");
    }

    #[test]
    fn body_text_pretty_prints_json_and_truncates() {
        assert!(body_text(r#"{"a":1}"#, false, "application/json").contains('\n'));
        assert_eq!(body_text("aGk=", true, ""), "(binary, 4 bytes base64)");
        let long = "x".repeat(MAX_BODY_SHOWN + 10);
        assert!(body_text(&long, false, "text/plain").ends_with("(truncated)"));
    }

    #[test]
    fn flatten_meta_dots_nested_keys_and_skips_empties() {
        let rows = flatten_meta(
            r#"{"response":{"protocol":"h2","remoteIPAddress":"1.2.3.4","x":"","tls":{"san":["a","b"]}},"timingMs":{"dns":1.5},"n":null}"#,
        );
        assert!(rows.contains(&("response.protocol".into(), "h2".into())));
        assert!(rows.contains(&("response.tls.san".into(), "a, b".into())));
        assert!(rows.contains(&("timingMs.dns".into(), "1.5".into())));
        assert!(!rows.iter().any(|(k, _)| k.ends_with(".x") || k == "n"));
    }

    #[test]
    fn registrable_domains() {
        assert_eq!(registrable_domain("api.ornn.com"), "ornn.com");
        assert_eq!(registrable_domain("ornn.com"), "ornn.com");
        assert_eq!(
            registrable_domain("a.b.shop.example.co.uk"),
            "example.co.uk"
        );
        assert_eq!(registrable_domain("WWW.Example.com.au"), "example.com.au");
        assert_eq!(registrable_domain("localhost"), "localhost");
        assert_eq!(registrable_domain("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn host_rows_lists_providers_then_each_ip() {
        use germal_core::hostinfo::{HostInfo, IpInfo};
        let info = HostInfo {
            host: "x.com".into(),
            ipv4: vec![IpInfo {
                ip: Some("1.2.3.4".parse().unwrap()),
                asn: Some(13335),
                provider: Some("CLOUDFLARENET, US".into()),
                ptr: Some("a.b".into()),
                ..Default::default()
            }],
            ipv6: vec![IpInfo {
                ip: Some("2606::1".parse().unwrap()),
                provider: Some("CLOUDFLARENET, US".into()),
                ..Default::default()
            }],
            nameservers: vec!["ns1.x.com".into()],
            error: None,
        };
        let rows = host_rows(&info);
        assert_eq!(rows[0], ("Provider".into(), "CLOUDFLARENET, US".into()));
        assert_eq!(rows[1].0, "DNS servers");
        assert_eq!(
            rows[2],
            (
                "IPv4".into(),
                "1.2.3.4 · AS13335 · CLOUDFLARENET, US · PTR a.b".into()
            )
        );
        assert_eq!(rows[3].0, "IPv6");
    }

    #[test]
    fn duration_scales_from_seconds_to_days() {
        let d = Duration::from_secs;
        assert_eq!(fmt_duration(Duration::from_millis(420)), "0.4 s");
        assert_eq!(fmt_duration(Duration::from_millis(45_200)), "45.2 s");
        assert_eq!(fmt_duration(d(125)), "2m 05s");
        assert_eq!(fmt_duration(d(3725)), "1h 02m 05s");
        assert_eq!(fmt_duration(d(93_784)), "1d 02h 03m 04s");
    }

    #[test]
    fn elapsed_formats_hms() {
        assert_eq!(
            fmt_elapsed(std::time::Duration::from_secs(3725)),
            "01:02:05"
        );
    }

    #[test]
    fn auth_headers_are_found_case_insensitively() {
        assert!(is_auth_header("Authorization"));
        assert!(!is_auth_header("accept"));
    }

    #[test]
    fn summary_lists_counts_failures_and_host_rows() {
        let mut r = LoadReport {
            planned: 10,
            sent: 10,
            total: 10,
            completed: 9,
            errors: 1,
            ..Default::default()
        };
        r.statuses.insert(200, 8);
        r.statuses.insert(500, 1);
        r.error_kinds.push(("timeout".into(), 1));
        let text = summary_text(
            "GET https://x.test/",
            &r,
            &[("IPv4".into(), "1.2.3.4".into())],
        );
        assert!(text.contains("failed 2"));
        assert!(text.contains("200×8, 500×1"));
        assert!(text.contains("error: timeout ×1"));
        assert!(text.contains("IPv4: 1.2.3.4"));
    }
}
