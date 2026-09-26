//! 录制控制器：启停一个有界面的 Chromium，把浏览时的 xhr / fetch 请求记进本地 SQLite
//! （数据目录下的 `recordings.db`，按 GET / POST / OTHER 分类）。
//!
//! 它不画界面：按钮与计时在主区 [`crate::ui::record_view`] 的顶栏，页面列表在左侧栏
//! [`crate::ui::record_pages`]。视图订阅这里的 [`RecordEvent`] 决定何时开始 / 结束拉新行。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::*;
use tokio::sync::oneshot;

use crate::bridge;
use crate::i18n::tr;

pub const DB_FILE: &str = "recordings.db";

enum State {
    Idle,
    /// 发送端 drop（退出）也会让录制收尾，见 `bridge::record`。
    Recording(oneshot::Sender<()>),
    /// 已通知停止，浏览器还在关。期间不许再开始，免得两次录制抢同一个库。
    Stopping,
    Finished,
    Failed(SharedString),
}

pub enum RecordEvent {
    Started,
    /// 录制已结束（正常 / 失败）；此后再拉一次就是最后一批。
    Stopped,
}

impl EventEmitter<RecordEvent> for RecordSheet {}

pub struct RecordSheet {
    state: State,
    started: Option<Instant>,
    ended: Option<Instant>,
    _run: Option<Task<()>>,
}

impl RecordSheet {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            started: None,
            ended: None,
            _run: None,
        }
    }

    pub fn is_recording(&self) -> bool {
        matches!(self.state, State::Recording(_))
    }

    pub fn is_stopping(&self) -> bool {
        matches!(self.state, State::Stopping)
    }

    pub fn error(&self) -> Option<&SharedString> {
        match &self.state {
            State::Failed(m) => Some(m),
            _ => None,
        }
    }

    /// 本次录制已运行多久：进行中随时间增长，结束后定格。
    pub fn elapsed(&self) -> Option<Duration> {
        let start = self.started?;
        Some(self.ended.unwrap_or_else(Instant::now) - start)
    }

    fn start(&mut self, db: PathBuf, project: i64, cx: &mut Context<Self>) {
        let (tx, rx) = oneshot::channel();
        let run = bridge::record(cx, db, project, rx);
        self.state = State::Recording(tx);
        (self.started, self.ended) = (Some(Instant::now()), None);
        self._run = Some(cx.spawn(async move |this, cx| {
            let result = run.await;
            let _ = this.update(cx, |this, cx| {
                this.ended = Some(Instant::now());
                this.state = match result {
                    Ok(_) => State::Finished,
                    // 启动失败（没装 Chrome 等）是真错误；用户直接关掉浏览器窗口是 Ok
                    Err(e) => {
                        tracing::error!("recorder failed: {e:#}");
                        State::Failed(format!("{e:#}").into())
                    }
                };
                cx.emit(RecordEvent::Stopped);
                cx.notify();
            });
        }));
        cx.emit(RecordEvent::Started);
    }

    /// 开始录制（录进 `project`）或停止。
    pub fn toggle(&mut self, project: i64, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Recording(tx) => {
                let _ = tx.send(());
                self.state = State::Stopping;
            }
            State::Stopping => self.state = State::Stopping,
            other => {
                self.state = other;
                if let Some(root) = crate::state::store::store(cx).map(|s| s.root().to_path_buf()) {
                    self.start(root.join(DB_FILE), project, cx);
                } else {
                    self.state = State::Failed(tr!("tools.recorder.no_store"));
                }
            }
        }
        cx.notify();
    }
}
