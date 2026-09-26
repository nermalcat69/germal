//! tokio ⇄ gpui 桥接。
use std::path::PathBuf;

use germal_core::http::{
    self, BodyStore, HttpClients, HttpRequest, HttpResponse, RequestError, StreamEvent,
};
use germal_core::model::{HttpVersionPref, RequestSettings};
use germal_core::store::{copy_atomic_user, write_atomic_user};
use gpui_kit::{App, Global, Task};
use gpui_tokio::Tokio;
use tokio::sync::mpsc;

/// 全局共享的 reqwest Client 组；发送流程按 Tab 选的 HTTP 版本从这里取用。
pub struct HttpClient(pub HttpClients);
impl Global for HttpClient {}

pub fn init(cx: &mut App) {
    gpui_tokio::init(cx);
    cx.set_global(HttpClient(http::build_clients()));
}

/// 按请求设置重建全局 client（设置改动后调用）。正在进行的请求继续用旧 client 直到结束。
pub fn rebuild_client(cx: &mut App, settings: &RequestSettings) {
    cx.set_global(HttpClient(http::build_clients_with(settings)));
}

/// 在 tokio runtime 上执行请求；返回的 gpui Task 被 drop 时底层 tokio 任务自动 abort。
pub fn send(
    cx: &App,
    req: HttpRequest,
    version: HttpVersionPref,
    progress: mpsc::Sender<StreamEvent>,
) -> Task<anyhow::Result<Result<HttpResponse, RequestError>>> {
    let client = cx.global::<HttpClient>().0.get(version).clone();
    Tokio::spawn_result(cx, async move {
        Ok(http::execute(&client, req, Some(progress)).await)
    })
}

/// 在 tokio 的阻塞线程池上把响应体**原子**写到 `dest`：Memory 走 `write_atomic_user`，
/// Spilled 走 `copy_atomic_user`（同目录临时文件 → fsync → rename）。中途失败不会留下半个目标文件；
/// drop 返回的 Task 只是不再等待结果（阻塞任务本身无法打断），目标路径仍然要么完整要么不变。
/// 用 `*_user` 变体：用户显式选择的"另存为"目标按系统 umask 创建（通常 0644），
/// 不继承数据目录内部文件的 0600（见 Ruling P4-3）。
pub fn save_body(cx: &App, body: BodyStore, dest: PathBuf) -> Task<anyhow::Result<()>> {
    Tokio::spawn_result(cx, async move {
        tokio::task::spawn_blocking(move || match &body {
            BodyStore::Memory(bytes) => write_atomic_user(&dest, bytes),
            // `body` 持有 Arc<SpillFile>，拷贝期间临时文件不会被删除
            BodyStore::Spilled { file, .. } => copy_atomic_user(file.path(), &dest),
        })
        .await??;
        Ok(())
    })
}

/// 在 tokio 上对同一条请求做压测；进度写进共享的 `progress`，界面随时取快照。
/// 丢弃返回的 Task = 取消：工作任务随 future 一起中止。
pub fn load_test(
    cx: &App,
    req: HttpRequest,
    version: HttpVersionPref,
    total: u64,
    concurrency: usize,
    progress: std::sync::Arc<germal_core::loadtest::Progress>,
) -> Task<anyhow::Result<germal_core::loadtest::LoadReport>> {
    let client = cx.global::<HttpClient>().0.get(version).clone();
    Tokio::spawn_result(cx, async move {
        Ok(germal_core::loadtest::run_live(&client, &req, total, concurrency, progress).await)
    })
}

/// 查主机的 IPv4 / IPv6、反向域名、托管服务商（ASN）与 NS。
pub fn host_info(cx: &App, host: String) -> Task<anyhow::Result<germal_core::hostinfo::HostInfo>> {
    Tokio::spawn_result(
        cx,
        async move { Ok(germal_core::hostinfo::lookup(&host).await) },
    )
}

/// 启动录制（弹出 Chromium）；`stop` 收到信号或被丢弃时收尾，返回本次录到的条数。
pub fn record(
    cx: &App,
    db: PathBuf,
    project: i64,
    stop: tokio::sync::oneshot::Receiver<()>,
) -> Task<anyhow::Result<i64>> {
    Tokio::spawn_result(cx, async move {
        germal_recorder::record::run(&db, false, None, project, async {
            let _ = stop.await;
        })
        .await
    })
}

/// 录制库的行（不含请求 / 响应体）：`after` 为 None 取最新一批历史，否则只取 id 更大的新行。
pub fn list_entries(
    cx: &App,
    db: PathBuf,
    project: i64,
    after: Option<i64>,
) -> Task<anyhow::Result<Vec<germal_recorder::db::Entry>>> {
    Tokio::spawn_result(cx, async move {
        tokio::task::spawn_blocking(move || {
            let db = germal_recorder::db::Db::open(&db)?;
            Ok(match after {
                None => db.list_latest(project, 20_000)?,
                Some(a) => db.list_after(project, a, 5_000)?,
            })
        })
        .await?
    })
}

/// 完整的一行（带请求 / 响应体）。
pub fn load_entry(
    cx: &App,
    db: PathBuf,
    id: i64,
) -> Task<anyhow::Result<Option<germal_recorder::db::Entry>>> {
    Tokio::spawn_result(cx, async move {
        tokio::task::spawn_blocking(move || Ok(germal_recorder::db::Db::open(&db)?.get(id)?))
            .await?
    })
}

/// 项目列表。
pub fn list_projects(
    cx: &App,
    db: PathBuf,
) -> Task<anyhow::Result<Vec<germal_recorder::db::Project>>> {
    Tokio::spawn_result(cx, async move {
        tokio::task::spawn_blocking(
            move || Ok(germal_recorder::db::Db::open(&db)?.list_projects()?),
        )
        .await?
    })
}

/// 新建项目（同名返回已有的），返回它的 id。
pub fn create_project(cx: &App, db: PathBuf, name: String) -> Task<anyhow::Result<i64>> {
    Tokio::spawn_result(cx, async move {
        tokio::task::spawn_blocking(move || {
            germal_recorder::db::Db::open(&db)?.create_project(&name)
        })
        .await?
    })
}
