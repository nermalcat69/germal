//! 主机信息：域名解析出的 IPv4 / IPv6、每个 IP 的反向域名，以及托管它的服务商。
//!
//! 服务商用 IP 所属的 ASN 表示，查询走 Team Cymru 的「IP → ASN」DNS 接口（纯 DNS，没有 HTTP API、
//! 不用密钥）：`<反写的IP>.origin.asn.cymru.com` 的 TXT 给出 ASN 与网段，
//! `AS<号>.asn.cymru.com` 的 TXT 给出运营商名。另外查域名的 NS 记录，得到它用的 DNS 服务商。
//!
//! 全部查询各自带超时，失败只影响对应字段，不会让整个结果丢掉。

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use futures::future::join_all;
use hickory_resolver::TokioResolver;
use hickory_resolver::lookup::Lookup;
use hickory_resolver::proto::rr::RData;

/// 一次查询的应答里所有 TXT / PTR / NS 记录的文本形式。
fn texts(l: &Lookup) -> Vec<String> {
    l.answers()
        .iter()
        .filter_map(|r| match &r.data {
            RData::TXT(t) => Some(t.to_string()),
            RData::PTR(n) => Some(n.0.to_string()),
            RData::NS(n) => Some(n.0.to_string()),
            _ => None,
        })
        .collect()
}

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IpInfo {
    pub ip: Option<IpAddr>,
    /// 反向解析出的域名（PTR）
    pub ptr: Option<String>,
    pub asn: Option<u32>,
    /// 运营商名，如 `CLOUDFLARENET, US`
    pub provider: Option<String>,
    /// IP 所在的网段，如 `104.16.0.0/12`
    pub prefix: Option<String>,
    pub country: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostInfo {
    pub host: String,
    pub ipv4: Vec<IpInfo>,
    pub ipv6: Vec<IpInfo>,
    /// 域名所在区的权威 NS（DNS 服务商）
    pub nameservers: Vec<String>,
    pub error: Option<String>,
}

/// `1.2.3.4` → `4.3.2.1.origin.asn.cymru.com`；IPv6 按半字节（nibble）反写，走 `origin6`。
fn cymru_origin_name(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.{}.origin.asn.cymru.com", o[3], o[2], o[1], o[0])
        }
        IpAddr::V6(v6) => {
            let nibbles: Vec<String> = v6
                .octets()
                .iter()
                .rev()
                .flat_map(|b| [format!("{:x}", b & 0xf), format!("{:x}", b >> 4)])
                .collect();
            format!("{}.origin6.asn.cymru.com", nibbles.join("."))
        }
    }
}

/// `"13335 | 104.16.0.0/12 | US | arin | 2014-03-28"` → (ASN, 网段, 国家)。多 ASN 取第一个。
fn parse_origin(txt: &str) -> Option<(u32, String, String)> {
    let mut f = txt.split('|').map(str::trim);
    let asn = f.next()?.split_whitespace().next()?.parse().ok()?;
    Some((asn, f.next()?.to_string(), f.next()?.to_string()))
}

/// `"13335 | US | arin | 2010-07-14 | CLOUDFLARENET, US"` → 运营商名（第 5 段）。
fn parse_asn_name(txt: &str) -> Option<String> {
    txt.split('|')
        .nth(4)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn txt(r: &TokioResolver, name: &str) -> Option<String> {
    let l = tokio::time::timeout(TIMEOUT, r.txt_lookup(name))
        .await
        .ok()?
        .ok()?;
    // 一条 TXT 记录可能被切成多段字符串，拼回去
    texts(&l).into_iter().next()
}

async fn ip_info(
    r: &TokioResolver,
    ip: IpAddr,
    asn_names: &tokio::sync::Mutex<HashMap<u32, Option<String>>>,
) -> IpInfo {
    let mut info = IpInfo {
        ip: Some(ip),
        ..Default::default()
    };
    let origin_name = cymru_origin_name(ip);
    let (ptr, origin) = tokio::join!(
        async {
            let l = tokio::time::timeout(TIMEOUT, r.reverse_lookup(ip))
                .await
                .ok()?
                .ok()?;
            texts(&l)
                .into_iter()
                .next()
                .map(|n| n.trim_end_matches('.').to_string())
        },
        txt(r, &origin_name),
    );
    info.ptr = ptr;
    if let Some((asn, prefix, cc)) = origin.as_deref().and_then(parse_origin) {
        info.asn = Some(asn);
        info.prefix = Some(prefix);
        info.country = Some(cc).filter(|c| !c.is_empty());
        let cached = asn_names.lock().await.get(&asn).cloned();
        info.provider = match cached {
            Some(name) => name,
            None => {
                let name = txt(r, &format!("AS{asn}.asn.cymru.com"))
                    .await
                    .as_deref()
                    .and_then(parse_asn_name);
                asn_names.lock().await.insert(asn, name.clone());
                name
            }
        };
    }
    info
}

/// 域名的权威 NS：从完整域名往上找，第一个有 NS 记录的区（至少保留两段，不查 TLD）。
async fn nameservers(r: &TokioResolver, host: &str) -> Vec<String> {
    let labels: Vec<&str> = host.trim_end_matches('.').split('.').collect();
    for start in 0..labels.len().saturating_sub(1) {
        let zone = labels[start..].join(".");
        if let Ok(Ok(l)) = tokio::time::timeout(TIMEOUT, r.ns_lookup(zone.as_str())).await {
            let mut ns: Vec<String> = texts(&l)
                .into_iter()
                .map(|n| n.trim_end_matches('.').to_string())
                .collect();
            ns.sort();
            ns.dedup();
            if !ns.is_empty() {
                return ns;
            }
        }
    }
    Vec::new()
}

/// 查一个主机名（或 IP 字面量）。永不 panic、永不整体失败：出错记在 `error`。
pub async fn lookup(host: &str) -> HostInfo {
    let mut out = HostInfo {
        host: host.to_string(),
        ..Default::default()
    };
    let resolver = match TokioResolver::builder_tokio().and_then(|b| b.build()) {
        Ok(r) => r,
        Err(e) => {
            out.error = Some(format!("resolver: {e}"));
            return out;
        }
    };
    let ips: Vec<IpAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![ip]
    } else {
        match tokio::time::timeout(TIMEOUT, resolver.lookup_ip(host)).await {
            Ok(Ok(l)) => {
                let mut v: Vec<IpAddr> = l.iter().collect();
                v.sort();
                v.dedup();
                v
            }
            Ok(Err(e)) => {
                out.error = Some(e.to_string());
                Vec::new()
            }
            Err(_) => {
                out.error = Some("DNS lookup timed out".into());
                Vec::new()
            }
        }
    };
    let names = tokio::sync::Mutex::new(HashMap::new());
    let (infos, ns) = tokio::join!(
        join_all(ips.iter().map(|ip| ip_info(&resolver, *ip, &names))),
        async {
            if host.parse::<IpAddr>().is_ok() {
                Vec::new()
            } else {
                nameservers(&resolver, host).await
            }
        }
    );
    for info in infos {
        match info.ip {
            Some(IpAddr::V4(_)) => out.ipv4.push(info),
            _ => out.ipv6.push(info),
        }
    }
    out.nameservers = ns;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cymru_names() {
        assert_eq!(
            cymru_origin_name("104.16.1.2".parse().unwrap()),
            "2.1.16.104.origin.asn.cymru.com"
        );
        assert_eq!(
            cymru_origin_name("2001:db8::1".parse().unwrap()),
            "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.origin6.asn.cymru.com"
        );
    }

    #[test]
    fn parses_cymru_txt() {
        assert_eq!(
            parse_origin("13335 | 104.16.0.0/12 | US | arin | 2014-03-28"),
            Some((13335, "104.16.0.0/12".into(), "US".into()))
        );
        assert_eq!(
            parse_origin("13335 15169 | 1.0.0.0/24 | AU | apnic | x")
                .unwrap()
                .0,
            13335
        );
        assert_eq!(
            parse_asn_name("13335 | US | arin | 2010-07-14 | CLOUDFLARENET, US"),
            Some("CLOUDFLARENET, US".into())
        );
        assert_eq!(parse_origin("garbage"), None);
    }

    /// 需要联网：`cargo test -p germal-core hostinfo -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs network"]
    async fn live_lookup_finds_ips_and_provider() {
        let info = lookup("one.one.one.one").await;
        println!("{info:#?}");
        assert!(!info.ipv4.is_empty());
        assert!(info.ipv4.iter().any(|i| i.provider.is_some()));
    }
}
