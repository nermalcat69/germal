//! 压测控制器 + 左侧栏面板：选定目标请求、设请求数与并发、开始 / 取消。
//!
//! 结果不在这里画：主区的 [`crate::ui::load_view`] 读这个实体的状态，实时显示已发 / 完成 / 失败、
//! 状态码分布、延迟与目标主机信息。面板取不到当前 Tab 的草稿，所以按钮只发 [`LoadEvent`]，
//! 由 `Workspace` 订阅后把展开好变量、校验过的请求交回 [`LoadSheet::start`]。

use std::sync::Arc;
use std::time::Duration;

use germal_core::hostinfo::HostInfo;
use germal_core::http::HttpRequest;
use germal_core::loadtest::{LoadReport, Progress};
use germal_core::model::{HttpVersionPref, Method, RequestDraft};
use gpui_kit::base::SelectableText;
use gpui_kit::component::{
    ActiveTheme, Disableable, Selectable, Sizable,
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::*;

use crate::bridge;
use crate::i18n::tr;

const DEFAULT_REQUESTS: &str = "100";
const DEFAULT_CONCURRENCY: &str = "10";
/// 上限只是防手滑：一次点错 9 个 0 不至于把本机与对端打挂。
const MAX_REQUESTS: u64 = 1_000_000;
const MAX_CONCURRENCY: usize = 1_000;
const TICK: Duration = Duration::from_millis(200);

/// 压测对象：草稿保持原样（变量在每次开始时才展开），标签与主机名取自展开后的结果。
#[derive(Clone)]
pub struct Target {
    pub draft: RequestDraft,
    pub group: Option<String>,
    pub version: HttpVersionPref,
    /// 变量展开、参数拼好之后的完整 URL；输入框以它为初值，用户改了才覆盖草稿
    pub url: String,
    pub host: Option<String>,
}

#[derive(Clone, PartialEq)]
pub enum Phase {
    Idle,
    Running,
    Done,
    Cancelled,
    Failed(SharedString),
}

#[derive(Clone)]
pub enum HostState {
    Unknown,
    Loading,
    Done(HostInfo),
}

/// 面板 → 宿主。
pub enum LoadEvent {
    Run,
    UseCurrentTab,
}

impl EventEmitter<LoadEvent> for LoadSheet {}

pub struct LoadSheet {
    /// 可编辑的目标 URL 与方法：不必回到 Tab 里改再点「使用当前 Tab」
    url: Entity<InputState>,
    method: Method,
    /// 应用到输入框的初值（渲染时才有 window，所以先记下来）
    pending_url: Option<String>,
    base_url: String,
    looked_up: Option<String>,
    _sub: Subscription,
    requests: Entity<InputState>,
    concurrency: Entity<InputState>,
    target: Option<Target>,
    phase: Phase,
    progress: Option<Arc<Progress>>,
    /// 最近一份汇总：运行中每 200 ms 刷新，结束后是最终结果
    report: Option<LoadReport>,
    host: HostState,
    task: Option<Task<()>>,
    ticker: Option<Task<()>>,
    host_task: Option<Task<()>>,
}

impl LoadSheet {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = |v: &str, window: &mut Window, cx: &mut Context<Self>| {
            let v = v.to_string();
            cx.new(|cx| InputState::new(window, cx).default_value(v))
        };
        let url =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://api.example.com/path"));
        // 改完 URL（回车 / 失焦）后重查新主机的 IP 与服务商
        let sub = cx.subscribe_in(&url, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                this.refresh_host(cx);
            }
        });
        Self {
            url,
            method: Method::Get,
            pending_url: None,
            base_url: String::new(),
            looked_up: None,
            _sub: sub,
            requests: field(DEFAULT_REQUESTS, window, cx),
            concurrency: field(DEFAULT_CONCURRENCY, window, cx),
            target: None,
            phase: Phase::Idle,
            progress: None,
            report: None,
            host: HostState::Unknown,
            task: None,
            ticker: None,
            host_task: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.phase == Phase::Running
    }

    pub fn target(&self) -> Option<&Target> {
        self.target.as_ref()
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn report(&self) -> Option<&LoadReport> {
        self.report.as_ref()
    }

    pub fn host(&self) -> &HostState {
        &self.host
    }

    #[cfg(test)]
    pub fn set_url_for_test(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.url
            .update(cx, |i, cx| i.set_value(url.to_string(), window, cx));
    }

    pub fn fail(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.phase = Phase::Failed(message.into());
        cx.notify();
    }

    /// 目标 URL 输入框里当前的文字。
    pub fn current_url(&self, cx: &App) -> String {
        self.url.read(cx).value().trim().to_string()
    }

    /// 标题用：方法 + 当前 URL。
    pub fn label(&self, cx: &App) -> SharedString {
        format!("{} {}", self.method.as_str(), self.current_url(cx)).into()
    }

    /// 用户改过方法或 URL 时返回新值；没改就是 None（保留草稿原样，动态变量照常每次展开）。
    pub fn overrides(&self, cx: &App) -> Option<(Method, String)> {
        let t = self.target.as_ref()?;
        let url = self.current_url(cx);
        (self.method != t.draft.method || url != self.base_url).then_some((self.method, url))
    }

    /// 换压测对象：清掉上一次的结果，并查新目标主机的 IP / 服务商。运行中不允许换。
    pub fn set_target(&mut self, target: Target, cx: &mut Context<Self>) {
        if self.is_running() {
            return;
        }
        self.method = target.draft.method;
        self.base_url = target.url.clone();
        self.pending_url = Some(target.url.clone());
        let host = target.host.clone();
        self.target = Some(target);
        (self.phase, self.report, self.progress) = (Phase::Idle, None, None);
        self.looked_up = None;
        self.host = HostState::Unknown;
        self.host_task = None;
        if let Some(host) = host {
            self.lookup_host(host, cx);
        }
        cx.notify();
    }

    fn lookup_host(&mut self, host: String, cx: &mut Context<Self>) {
        self.looked_up = Some(host.clone());
        self.host = HostState::Loading;
        let lookup = bridge::host_info(cx, host);
        self.host_task = Some(cx.spawn(async move |this, cx| {
            let info = lookup.await.ok();
            let _ = this.update(cx, |this, cx| {
                this.host = info.map_or(HostState::Unknown, HostState::Done);
                cx.notify();
            });
        }));
    }

    /// URL 被改后：主机变了就重查。含未展开变量（`{{host}}`）的 URL 解析不出主机，保持原样。
    fn refresh_host(&mut self, cx: &mut Context<Self>) {
        if self.is_running() {
            return;
        }
        let probe = RequestDraft {
            url: self.current_url(cx),
            ..Default::default()
        };
        let host = germal_core::url::build_url(&probe)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string));
        if let Some(host) = host
            && self.looked_up.as_deref() != Some(host.as_str())
        {
            self.lookup_host(host, cx);
            cx.notify();
        }
    }

    /// 读输入框的数并开始；数不合法就地报错，不发请求。
    pub fn start(&mut self, req: HttpRequest, cx: &mut Context<Self>) {
        let Some(version) = self.target.as_ref().map(|t| t.version) else {
            return;
        };
        if self.is_running() {
            return;
        }
        let total = self.requests.read(cx).value().trim().parse::<u64>();
        let conc = self.concurrency.read(cx).value().trim().parse::<usize>();
        let (Ok(total @ 1..=MAX_REQUESTS), Ok(conc @ 1..=MAX_CONCURRENCY)) = (total, conc) else {
            return self.fail(
                tr!(
                    "tools.load_test.invalid",
                    max_requests = MAX_REQUESTS,
                    max_concurrency = MAX_CONCURRENCY
                ),
                cx,
            );
        };
        let progress = Progress::new(total);
        self.progress = Some(progress.clone());
        self.report = Some(progress.snapshot());
        self.phase = Phase::Running;
        let run = bridge::load_test(cx, req, version, total, conc, progress.clone());
        // 存下 Task：取消 / 再次开始时 drop 它，tokio 侧的压测随之 abort
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = run.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(report) => {
                        this.report = Some(report);
                        this.phase = Phase::Done;
                    }
                    Err(e) => this.phase = Phase::Failed(e.to_string().into()),
                }
                this.ticker = None;
                cx.notify();
            });
        }));
        // 运行期间每 200 ms 取一次快照，界面因此「边跑边显示」
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let keep = this.update(cx, |this, cx| {
                    if let (true, Some(p)) = (this.is_running(), &this.progress) {
                        this.report = Some(p.snapshot());
                        cx.notify();
                        true
                    } else {
                        false
                    }
                });
                if !keep.unwrap_or(false) {
                    return;
                }
            }
        }));
        cx.notify();
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.task.take().is_some() && self.is_running() {
            self.ticker = None;
            // 保留已完成部分的汇总，标成「已取消」
            if let Some(p) = &self.progress {
                self.report = Some(p.snapshot());
            }
            self.phase = Phase::Cancelled;
            cx.notify();
        }
    }
}

impl Render for LoadSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(url) = self.pending_url.take() {
            self.url.update(cx, |i, cx| i.set_value(url, window, cx));
        }
        let running = self.is_running();
        let muted = cx.theme().muted_foreground;
        let labeled = |label: SharedString, input: &Entity<InputState>| {
            v_flex()
                .gap_1()
                .child(div().text_xs().text_color(muted).child(label))
                .child(Input::new(input).small().disabled(running))
        };
        let target = if self.target.is_some() {
            v_flex()
                .gap_2()
                .child(h_flex().gap_1().flex_wrap().children(Method::ALL.map(|m| {
                    Button::new(("load-method", m as usize))
                        .ghost()
                        .xsmall()
                        .disabled(running)
                        .selected(self.method == m)
                        .label(m.as_str())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.method = m;
                            cx.notify();
                        }))
                })))
                .child(Input::new(&self.url).small().disabled(running))
                .into_any_element()
        } else {
            div()
                .text_sm()
                .text_color(muted)
                .child(tr!("tools.load_test.no_target"))
                .into_any_element()
        };
        v_flex()
            .id("load-panel")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3()
            .pb_3()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tr!("tools.load_test.target")),
                    )
                    .child(target),
            )
            .child(
                Button::new("load-use-tab")
                    .small()
                    .disabled(running)
                    .label(tr!("tools.load_test.use_tab"))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(LoadEvent::UseCurrentTab))),
            )
            .child(labeled(tr!("tools.load_test.requests"), &self.requests))
            .child(labeled(
                tr!("tools.load_test.concurrency"),
                &self.concurrency,
            ))
            .child(
                Button::new("load-run")
                    .primary()
                    .small()
                    .disabled(self.target.is_none())
                    .label(if running {
                        tr!("tools.load_test.cancel")
                    } else {
                        tr!("tools.load_test.start")
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.is_running() {
                            this.cancel(cx);
                        } else {
                            cx.emit(LoadEvent::Run);
                        }
                    })),
            )
            .children(match &self.phase {
                Phase::Failed(m) => Some(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(SelectableText::new("load-error", m.clone())),
                ),
                _ => None,
            })
    }
}
