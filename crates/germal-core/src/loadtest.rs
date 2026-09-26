//! 压测：同一条请求按并发数发 N 次，汇总状态码分布、吞吐与延迟分位数。
//!
//! 进度是共享的 [`Progress`]：工作任务边跑边记，界面随时 [`Progress::snapshot`] 取一份
//! 当前汇总——所以「已发多少、完成多少、失败多少、各状态码」能在跑的过程中同步显示。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::Client;
use tokio::task::JoinSet;

use crate::http::{HttpRequest, execute};

/// 延迟样本上限：超过后只保留每隔 k 个的样本，分位数是近似值（百万级请求也不至于吃光内存）。
/// ponytail: 等间隔抽样；要精确分位数换 HDR histogram。
const MAX_SAMPLES: usize = 200_000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadReport {
    /// 计划发送的总数
    pub planned: u64,
    /// 已发出（含在途）
    pub sent: u64,
    /// 已有结果（成功拿到响应 + 传输失败）
    pub total: u64,
    /// 拿到了响应（任何状态码）。
    pub completed: u64,
    /// 传输层失败：超时、连接被拒等。
    pub errors: u64,
    pub in_flight: u64,
    pub statuses: BTreeMap<u16, u64>,
    /// 失败原因 → 次数，多的在前
    pub error_kinds: Vec<(String, u64)>,
    pub elapsed: Duration,
    pub rps: f64,
    pub min: Duration,
    pub p50: Duration,
    pub p90: Duration,
    pub p99: Duration,
    pub max: Duration,
}

impl LoadReport {
    /// 2xx / 3xx 之外的响应（4xx、5xx）也算「失败」显示给用户看：传输错误 + 非成功状态码。
    pub fn failed(&self) -> u64 {
        self.errors
            + self
                .statuses
                .iter()
                .filter(|(s, _)| !(200..400).contains(*s))
                .map(|(_, n)| n)
                .sum::<u64>()
    }
}

#[derive(Default)]
struct Inner {
    statuses: BTreeMap<u16, u64>,
    error_kinds: BTreeMap<String, u64>,
    /// 全部（或抽样后的）延迟
    lat: Vec<Duration>,
    /// 抽样步长，每次翻倍
    stride: usize,
    seen: usize,
}

pub struct Progress {
    planned: AtomicU64,
    sent: AtomicU64,
    in_flight: AtomicU64,
    inner: Mutex<Inner>,
    started: Instant,
}

impl Progress {
    pub fn new(planned: u64) -> Arc<Progress> {
        Arc::new(Progress {
            planned: AtomicU64::new(planned),
            sent: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                stride: 1,
                ..Default::default()
            }),
            started: Instant::now(),
        })
    }

    fn record(&self, status: Result<u16, String>, took: Duration) {
        let mut g = self.inner.lock().unwrap();
        match status {
            Ok(s) => *g.statuses.entry(s).or_default() += 1,
            Err(e) => *g.error_kinds.entry(e).or_default() += 1,
        }
        g.seen += 1;
        if g.seen.is_multiple_of(g.stride) {
            g.lat.push(took);
            if g.lat.len() >= MAX_SAMPLES {
                // 丢掉一半（隔一个留一个）并把步长翻倍
                let mut i = 0;
                g.lat.retain(|_| {
                    i += 1;
                    i % 2 == 0
                });
                g.stride *= 2;
            }
        }
    }

    /// 当前汇总。可随时调用；分位数需要排序，别在紧循环里调。
    pub fn snapshot(&self) -> LoadReport {
        let g = self.inner.lock().unwrap();
        let elapsed = self.started.elapsed();
        let completed: u64 = g.statuses.values().sum();
        let errors: u64 = g.error_kinds.values().sum();
        let total = completed + errors;
        let mut kinds: Vec<(String, u64)> =
            g.error_kinds.iter().map(|(k, v)| (k.clone(), *v)).collect();
        kinds.sort_by_key(|k| std::cmp::Reverse(k.1));
        let mut r = LoadReport {
            planned: self.planned.load(Ordering::Relaxed),
            sent: self.sent.load(Ordering::Relaxed),
            total,
            completed,
            errors,
            in_flight: self.in_flight.load(Ordering::Relaxed),
            statuses: g.statuses.clone(),
            error_kinds: kinds,
            elapsed,
            rps: total as f64 / elapsed.as_secs_f64().max(f64::EPSILON),
            ..Default::default()
        };
        let mut lat = g.lat.clone();
        drop(g);
        lat.sort();
        if let (Some(&min), Some(&max)) = (lat.first(), lat.last()) {
            // 最近秩法：第 ceil(p·n) 个样本
            let pct =
                |p: f64| lat[((p * lat.len() as f64).ceil() as usize).clamp(1, lat.len()) - 1];
            (r.min, r.max) = (min, max);
            (r.p50, r.p90, r.p99) = (pct(0.50), pct(0.90), pct(0.99));
        }
        r
    }
}

/// 发 `total` 次、最多 `concurrency` 个在途；边跑边写 `progress`。`concurrency` 为 0 按 1 算。
/// future 被丢弃（取消）时所有工作任务一并中止，`progress` 里留着已完成部分。
pub async fn run_live(
    client: &Client,
    req: &HttpRequest,
    total: u64,
    concurrency: usize,
    progress: Arc<Progress>,
) -> LoadReport {
    let req = Arc::new(req.clone());
    let next = Arc::new(AtomicU64::new(0));
    let mut workers = JoinSet::new();
    for _ in 0..concurrency.max(1) {
        let (client, req, next, progress) =
            (client.clone(), req.clone(), next.clone(), progress.clone());
        workers.spawn(async move {
            while next.fetch_add(1, Ordering::Relaxed) < total {
                progress.sent.fetch_add(1, Ordering::Relaxed);
                progress.in_flight.fetch_add(1, Ordering::Relaxed);
                let t = Instant::now();
                let result = execute(&client, (*req).clone(), None)
                    .await
                    .map(|r| r.meta.status)
                    .map_err(|e| e.to_string());
                progress.in_flight.fetch_sub(1, Ordering::Relaxed);
                progress.record(result, t.elapsed());
            }
        });
    }
    while workers.join_next().await.is_some() {}
    progress.snapshot()
}

/// 不关心过程、只要最终结果时用。
pub async fn run(client: &Client, req: &HttpRequest, total: u64, concurrency: usize) -> LoadReport {
    run_live(client, req, total, concurrency, Progress::new(total)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_counts_and_failures() {
        let ms = Duration::from_millis;
        let p = Progress::new(103);
        for i in 1..=100 {
            p.record(Ok(200), ms(i));
        }
        p.record(Ok(503), ms(5));
        p.record(Err("timeout".into()), ms(500));
        p.record(Err("timeout".into()), ms(500));
        let r = p.snapshot();
        assert_eq!((r.total, r.completed, r.errors), (103, 101, 2));
        assert_eq!(r.statuses[&200], 100);
        assert_eq!(r.failed(), 3); // 2 个传输错误 + 1 个 503
        assert_eq!(r.error_kinds, vec![("timeout".to_string(), 2)]);
        assert_eq!((r.min, r.max), (ms(1), ms(500)));
    }

    #[test]
    fn sample_cap_keeps_memory_bounded() {
        let p = Progress::new(0);
        for _ in 0..MAX_SAMPLES * 3 {
            p.record(Ok(200), Duration::from_millis(1));
        }
        assert!(p.inner.lock().unwrap().lat.len() < MAX_SAMPLES);
        assert_eq!(p.snapshot().total, (MAX_SAMPLES * 3) as u64);
    }
}
