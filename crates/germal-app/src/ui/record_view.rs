//! 录制的主区视图：左边可滚动的请求列表（新的在上），右边是选中那条的详情——
//! 端点、状态与耗时、认证头、Cookie、全部请求 / 响应头与正文。详情里的文字都能拖选、⌘C 复制。
//!
//! 数据来自 `recordings.db`：启动时载入最近一批历史，录制期间每秒拉一次新行（面板发
//! [`RecordEvent`] 通知开始 / 结束）。列表只持有精简的 [`Row`]，正文等点开再按 id 单取，
//! 这样每秒的刷新不会复制成千上万条大字符串。

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::Duration;

use germal_core::codegen::{self, CodeTarget};
use germal_core::model::{Method, RequestDraft};
use germal_recorder::db::{Entry, Project};
use gpui_kit::base::SelectableText;
use gpui_kit::component::{
    ActiveTheme, Disableable, Selectable, Sizable, WindowExt as _,
    button::{Button, ButtonVariants},
    clipboard::Clipboard,
    dialog::{DialogAction, DialogClose, DialogFooter},
    h_flex,
    input::{Input, InputState},
    menu::{ContextMenuExt as _, PopupMenuItem},
    scroll::Scrollbar,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::bridge;
use crate::i18n::tr;
use crate::ui::load_sheet::HostState;
use crate::ui::record_fmt::{
    body_text, flatten_meta, fmt_bytes, fmt_elapsed, fmt_ms, fmt_utc, header, host_rows,
    is_auth_header, parse_headers, registrable_domain,
};
use crate::ui::record_sheet::{DB_FILE, RecordEvent, RecordSheet};
use crate::ui::{method_color, status_color};

const POLL_EVERY: Duration = Duration::from_secs(1);
const DETAIL_WIDTH: f32 = 460.;

/// 列表行只带列表要画的字段。
#[derive(Clone)]
struct Row {
    id: i64,
    category: String,
    url: String,
    /// 注册域（`api.ornn.com` → `ornn.com`）
    domain: String,
    method: String,
    status: Option<u16>,
    host: String,
    path: String,
    duration_ms: Option<f64>,
    failed: bool,
}

impl From<&Entry> for Row {
    fn from(e: &Entry) -> Row {
        Row {
            id: e.id,
            category: e.category.clone(),
            url: e.url.clone(),
            domain: registrable_domain(&e.host),
            method: e.method.clone(),
            status: e.status,
            host: e.host.clone(),
            path: e.path.clone(),
            duration_ms: e.duration_ms,
            failed: e.error.is_some(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Get,
    Post,
    Other,
}

impl Filter {
    const ALL: [Filter; 4] = [Filter::All, Filter::Get, Filter::Post, Filter::Other];

    fn label(self) -> SharedString {
        match self {
            Filter::All => tr!("recorder.filter_all"),
            Filter::Get => "GET".into(),
            Filter::Post => "POST".into(),
            Filter::Other => tr!("recorder.filter_other"),
        }
    }

    fn matches(self, category: &str) -> bool {
        match self {
            Filter::All => true,
            Filter::Get => category == "GET",
            Filter::Post => category == "POST",
            Filter::Other => category == "OTHER",
        }
    }
}

/// 主区只看哪一部分：全部 / 某个域名（含其所有子域名）/ 某个具体主机。
#[derive(Clone, PartialEq, Eq)]
pub enum Scope {
    All,
    Domain(String),
    Host(String),
}

/// 左侧栏的一个域名及其下的子域名（含域名本身的主机）与请求数。
pub struct DomainNode {
    pub domain: String,
    pub total: usize,
    /// 域名本身在前，子域名按字母序
    pub hosts: Vec<(String, usize)>,
}

/// 视图 → 宿主：把这条录制送去压测。
pub enum RecordViewEvent {
    LoadTest(RequestDraft),
}

impl EventEmitter<RecordViewEvent> for RecordView {}

/// 录制行 → 可发送的草稿（头与正文照录，Cookie / Authorization 一并带上）。
fn draft_of(e: &Entry) -> Option<RequestDraft> {
    germal_recorder::replay::to_draft(&germal_recorder::replay::Row_ {
        id: e.id,
        method: e.method.clone(),
        url: e.url.clone(),
        host: e.host.clone(),
        status: e.status,
        req_headers: e.req_headers.clone(),
        post_data: e.post_data.clone(),
        imported: false,
    })
}

pub struct RecordView {
    hosts: HashMap<String, HostState>,
    sheet: Entity<RecordSheet>,
    /// 域名 → 主机 → 请求数（左侧栏按它组织）
    domains: BTreeMap<String, BTreeMap<String, usize>>,
    scope: Scope,
    projects: Vec<Project>,
    /// 当前项目；1 = 内置的 Default
    project: i64,
    /// id 升序
    rows: Rc<Vec<Row>>,
    /// 当前过滤器下可见的行下标，最新的在前
    visible: Rc<Vec<usize>>,
    last_id: i64,
    filter: Filter,
    selected: Option<i64>,
    detail: Option<Entry>,
    /// 正在录制：决定轮询循环是否继续
    active: bool,
    scroll: UniformListScrollHandle,
    detail_scroll: ScrollHandle,
    poll: Option<Task<()>>,
    load: Option<Task<()>>,
    _sub: Subscription,
}

fn db_path(cx: &App) -> Option<std::path::PathBuf> {
    crate::state::store::store(cx).map(|s| s.root().join(DB_FILE))
}

impl RecordView {
    pub fn new(sheet: &Entity<RecordSheet>, cx: &mut Context<Self>) -> Self {
        let sub = cx.subscribe(sheet, |this, _, event: &RecordEvent, cx| match event {
            RecordEvent::Started => {
                this.active = true;
                this.start_polling(cx);
            }
            RecordEvent::Stopped => this.active = false,
        });
        let mut this = Self {
            hosts: HashMap::new(),
            sheet: sheet.clone(),
            domains: BTreeMap::new(),
            scope: Scope::All,
            projects: Vec::new(),
            project: 1,
            rows: Rc::new(Vec::new()),
            visible: Rc::new(Vec::new()),
            last_id: 0,
            filter: Filter::All,
            selected: None,
            detail: None,
            active: false,
            scroll: UniformListScrollHandle::new(),
            detail_scroll: ScrollHandle::new(),
            poll: None,
            load: None,
            _sub: sub,
        };
        this.load_projects(cx);
        this.load_history(cx);
        this
    }

    fn load_history(&mut self, cx: &mut Context<Self>) {
        let Some(db) = db_path(cx) else { return };
        let task = bridge::list_entries(cx, db, self.project, None);
        self.load = Some(cx.spawn(async move |this, cx| {
            if let Ok(entries) = task.await {
                let _ = this.update(cx, |this, cx| this.push(&entries, cx));
            }
        }));
    }

    /// 录制期间每秒拉一次新行。先看「是否已结束」再拉：结束后的那一次拉到的就是最后一批。
    fn start_polling(&mut self, cx: &mut Context<Self>) {
        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_EVERY).await;
                let Ok((done, db, project, after)) = this.update(cx, |this, cx| {
                    // 每秒刷新一次，顶栏的已运行时长随之走动
                    cx.notify();
                    (!this.active, db_path(cx), this.project, this.last_id)
                }) else {
                    return;
                };
                if let Some(db) = db
                    && let Ok(entries) = cx
                        .update(|cx| bridge::list_entries(cx, db, project, Some(after)))
                        .await
                {
                    let _ = this.update(cx, |this, cx| this.push(&entries, cx));
                }
                if done {
                    return;
                }
            }
        }));
    }

    fn push(&mut self, entries: &[Entry], cx: &mut Context<Self>) {
        let fresh: Vec<Row> = entries
            .iter()
            .filter(|e| e.id > self.last_id)
            .map(Row::from)
            .collect();
        let Some(last) = fresh.last() else { return };
        self.last_id = last.id;
        for r in &fresh {
            *self
                .domains
                .entry(r.domain.clone())
                .or_default()
                .entry(r.host.clone())
                .or_default() += 1;
        }
        Rc::make_mut(&mut self.rows).extend(fresh);
        self.refilter();
        cx.notify();
    }

    fn refilter(&mut self) {
        self.visible = Rc::new(
            (0..self.rows.len())
                .rev()
                .filter(|&i| {
                    let r = &self.rows[i];
                    self.filter.matches(&r.category)
                        && match &self.scope {
                            Scope::All => true,
                            Scope::Domain(d) => r.domain == *d,
                            Scope::Host(h) => r.host == *h,
                        }
                })
                .collect(),
        );
    }

    /// 取完整的一行再做点什么（列表行只带摘要）。
    fn with_entry(
        &mut self,
        id: i64,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Self, Entry, &mut Context<Self>) + 'static,
    ) {
        let Some(db) = db_path(cx) else { return };
        let task = bridge::load_entry(cx, db, id);
        cx.spawn(async move |this, cx| {
            if let Ok(Some(entry)) = task.await {
                let _ = this.update(cx, |this, cx| f(this, entry, cx));
            }
        })
        .detach();
    }

    fn curl_of(cx: &App, e: &Entry) -> Option<String> {
        let disabled = crate::state::settings::settings(cx)
            .request
            .disabled_default_headers;
        codegen::generate(&draft_of(e)?, &disabled, CodeTarget::Curl).ok()
    }

    fn copy_curl(&mut self, id: i64, cx: &mut Context<Self>) {
        self.with_entry(id, cx, |_, e, cx| {
            if let Some(curl) = Self::curl_of(cx, &e) {
                cx.write_to_clipboard(ClipboardItem::new_string(curl));
            }
        });
    }

    fn copy_url(&mut self, id: i64, cx: &mut Context<Self>) {
        if let Some(r) = self.rows.iter().find(|r| r.id == id) {
            cx.write_to_clipboard(ClipboardItem::new_string(r.url.clone()));
        }
    }

    fn load_test(&mut self, id: i64, cx: &mut Context<Self>) {
        self.with_entry(id, cx, |_, e, cx| {
            if let Some(draft) = draft_of(&e) {
                cx.emit(RecordViewEvent::LoadTest(draft));
            }
        });
    }

    /// 选中一条后查它的主机信息（每个主机只查一次）。
    fn ensure_host(&mut self, host: &str, cx: &mut Context<Self>) {
        if host.is_empty() || self.hosts.contains_key(host) {
            return;
        }
        self.hosts.insert(host.to_string(), HostState::Loading);
        let key = host.to_string();
        let lookup = bridge::host_info(cx, key.clone());
        cx.spawn(async move |this, cx| {
            let info = lookup.await.ok();
            let _ = this.update(cx, |this, cx| {
                this.hosts
                    .insert(key, info.map_or(HostState::Unknown, HostState::Done));
                cx.notify();
            });
        })
        .detach();
    }

    /// 左侧栏的域名树：按域名字母序，域名下域名本身的主机在前、子域名按字母序。
    pub fn domain_tree(&self) -> Vec<DomainNode> {
        self.domains
            .iter()
            .map(|(domain, hosts)| {
                let mut hosts: Vec<(String, usize)> =
                    hosts.iter().map(|(h, n)| (h.clone(), *n)).collect();
                hosts.sort_by_key(|(h, _)| (h != domain, h.clone()));
                DomainNode {
                    domain: domain.clone(),
                    total: hosts.iter().map(|(_, n)| n).sum(),
                    hosts,
                }
            })
            .collect()
    }

    pub fn total(&self) -> usize {
        self.rows.len()
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn select_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        self.scope = scope;
        self.refilter();
        cx.notify();
    }

    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn project(&self) -> i64 {
        self.project
    }

    pub fn project_name(&self) -> String {
        self.projects
            .iter()
            .find(|p| p.id == self.project)
            .map_or_else(|| "Default".to_string(), |p| p.name.clone())
    }

    fn busy_recording(&self, cx: &App) -> bool {
        let s = self.sheet.read(cx);
        s.is_recording() || s.is_stopping()
    }

    fn load_projects(&mut self, cx: &mut Context<Self>) {
        let Some(db) = db_path(cx) else { return };
        let task = bridge::list_projects(cx, db);
        cx.spawn(async move |this, cx| {
            if let Ok(list) = task.await {
                let _ = this.update(cx, |this, cx| {
                    this.projects = list;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 切换项目：清空当前列表 / 选择，载入该项目的历史。录制期间不允许切（新请求会记进错的项目）。
    pub fn switch_project(&mut self, id: i64, cx: &mut Context<Self>) {
        if id == self.project || self.busy_recording(cx) {
            return;
        }
        self.project = id;
        self.rows = Rc::new(Vec::new());
        self.visible = Rc::new(Vec::new());
        self.domains.clear();
        self.last_id = 0;
        self.scope = Scope::All;
        self.selected = None;
        self.detail = None;
        self.load_history(cx);
        cx.notify();
    }

    fn create_project(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(db) = db_path(cx) else { return };
        let created = bridge::create_project(cx, db.clone(), name);
        cx.spawn(async move |this, cx| {
            // 空名字 / 数据库错误：静默忽略，对话框已关，列表保持原样
            let Ok(id) = created.await else { return };
            if let Ok(list) = cx.update(|cx| bridge::list_projects(cx, db)).await {
                let _ = this.update(cx, |this, cx| {
                    this.projects = list;
                    this.switch_project(id, cx);
                });
            }
        })
        .detach();
    }

    /// 「新建项目…」：输入名字的对话框。
    pub fn prompt_new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) || self.busy_recording(cx) {
            return;
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("recorder.project_name")));
        let weak = cx.entity().downgrade();
        let focus = input.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let content_input = input.clone();
            let ok_input = input.clone();
            let weak = weak.clone();
            dialog
                .title(tr!("recorder.new_project"))
                .content(move |content, _, _| content.child(Input::new(&content_input)))
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new().child(
                                Button::new("cancel-project")
                                    .outline()
                                    .label(tr!("common.cancel")),
                            ),
                        )
                        .child(
                            DialogAction::new().child(
                                Button::new("ok-project")
                                    .primary()
                                    .label(tr!("common.save")),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let name = ok_input.read(cx).value().to_string();
                    if let Some(v) = weak.upgrade() {
                        v.update(cx, |v, cx| v.create_project(name, cx));
                    }
                    true
                })
        });
        focus.update(cx, |s, cx| s.focus(window, cx));
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        self.refilter();
        cx.notify();
    }

    fn select(&mut self, id: i64, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.detail = None;
        let Some(db) = db_path(cx) else { return };
        let task = bridge::load_entry(cx, db, id);
        self.load = Some(cx.spawn(async move |this, cx| {
            let entry = task.await.ok().flatten();
            let _ = this.update(cx, |this, cx| {
                // 点得快时丢掉过期的结果
                if this.selected == Some(id) {
                    if let Some(e) = &entry {
                        let host = e.host.clone();
                        this.ensure_host(&host, cx);
                    }
                    this.detail = entry;
                    this.detail_scroll.set_offset(point(px(0.), px(0.)));
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }
}

impl Render for RecordView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .id("record-view")
            .size_full()
            .min_w_0()
            .child(
                h_flex()
                    .h_10()
                    .flex_none()
                    .px_4()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(self.render_record_button(cx))
                    .child(
                        div()
                            .w(px(72.))
                            .flex_none()
                            .text_sm()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(if self.sheet.read(cx).is_recording() {
                                cx.theme().danger
                            } else {
                                muted
                            })
                            .child(SharedString::from(
                                self.sheet
                                    .read(cx)
                                    .elapsed()
                                    .map(fmt_elapsed)
                                    .unwrap_or_else(|| "--:--:--".into()),
                            )),
                    )
                    .children(Filter::ALL.map(|f| {
                        Button::new(("record-filter", f as usize))
                            .ghost()
                            .xsmall()
                            .selected(self.filter == f)
                            .label(f.label())
                            .on_click(cx.listener(move |this, _, _, cx| this.set_filter(f, cx)))
                    }))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(tr!("recorder.count", count = self.visible.len())),
                    ),
            )
            .children(self.sheet.read(cx).error().cloned().map(|m| {
                div()
                    .px_4()
                    .py_1()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(SelectableText::new("record-error", m))
            }))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_list(cx))
                    .child(
                        div()
                            .id("record-detail")
                            .w(px(DETAIL_WIDTH))
                            .flex_none()
                            .h_full()
                            .border_l_1()
                            .border_color(cx.theme().border)
                            .overflow_y_scroll()
                            .track_scroll(&self.detail_scroll)
                            .child(match &self.detail {
                                Some(e) => self.render_detail(e, cx),
                                None => div()
                                    .p_4()
                                    .text_sm()
                                    .text_color(muted)
                                    .child(if self.selected.is_some() {
                                        tr!("recorder.loading")
                                    } else {
                                        tr!("recorder.pick_hint")
                                    })
                                    .into_any_element(),
                            }),
                    ),
            )
    }
}

impl RecordView {
    /// 顶栏的开始 / 停止按钮；录制中显示「停止」，停止中置灰。
    fn render_record_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (recording, stopping) = {
            let s = self.sheet.read(cx);
            (s.is_recording(), s.is_stopping())
        };
        Button::new("record-toggle")
            .small()
            .disabled(stopping)
            .map(|b| {
                if recording {
                    b.danger().label(tr!("tools.recorder.stop"))
                } else if stopping {
                    b.label(tr!("tools.recorder.stopping"))
                } else {
                    b.primary().label(tr!("tools.recorder.start"))
                }
            })
            .on_click(cx.listener(|this, _, _, cx| {
                let project = this.project;
                this.sheet.update(cx, |s, cx| s.toggle(project, cx));
            }))
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.visible.is_empty() {
            return div()
                .flex_1()
                .min_w_0()
                .p_4()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(tr!("recorder.empty"))
                .into_any_element();
        }
        let (rows, visible, selected) = (self.rows.clone(), self.visible.clone(), self.selected);
        let weak = cx.entity().downgrade();
        let list = uniform_list("record-rows", visible.len(), move |range, _window, cx| {
            let muted = cx.theme().muted_foreground;
            range
                .map(|ix| {
                    let row = &rows[visible[ix]];
                    let id = row.id;
                    let on = selected == Some(id);
                    let weak2 = weak.clone();
                    let weak = weak.clone();
                    let method_col = Method::parse(&row.method)
                        .map(|m| method_color(m, cx))
                        .unwrap_or(muted);
                    let (status_text, status_col) = match row.status {
                        Some(s) => (s.to_string(), status_color(s, cx)),
                        None => (
                            if row.failed {
                                "ERR".into()
                            } else {
                                "…".into()
                            },
                            cx.theme().danger,
                        ),
                    };
                    div().h_9().w_full().py_0p5().child(
                        h_flex()
                            .id(("record-row", ix))
                            .size_full()
                            .px_2()
                            .gap_2()
                            .items_center()
                            .rounded(cx.theme().radius)
                            .when(on, |r| r.bg(cx.theme().list_active))
                            .hover(|s| s.bg(cx.theme().list_hover))
                            .on_click(move |_, _, cx| {
                                if let Some(v) = weak.upgrade() {
                                    v.update(cx, |v, cx| v.select(id, cx));
                                }
                            })
                            .context_menu({
                                let weak = weak2;
                                move |menu, _, _| {
                                    let (a, b, c) = (weak.clone(), weak.clone(), weak.clone());
                                    menu.item(
                                        PopupMenuItem::new(tr!("recorder.menu_copy_url")).on_click(
                                            move |_, _, cx| {
                                                if let Some(v) = a.upgrade() {
                                                    v.update(cx, |v, cx| v.copy_url(id, cx));
                                                }
                                            },
                                        ),
                                    )
                                    .item(
                                        PopupMenuItem::new(tr!("recorder.menu_copy_curl"))
                                            .on_click(move |_, _, cx| {
                                                if let Some(v) = b.upgrade() {
                                                    v.update(cx, |v, cx| v.copy_curl(id, cx));
                                                }
                                            }),
                                    )
                                    .separator()
                                    .item(
                                        PopupMenuItem::new(tr!("recorder.menu_load_test"))
                                            .on_click(move |_, _, cx| {
                                                if let Some(v) = c.upgrade() {
                                                    v.update(cx, |v, cx| v.load_test(id, cx));
                                                }
                                            }),
                                    )
                                }
                            })
                            .child(
                                div()
                                    .w(px(64.))
                                    .flex_none()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(method_col)
                                    .child(SharedString::from(row.method.clone())),
                            )
                            .child(
                                div()
                                    .w_10()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(status_col)
                                    .child(SharedString::from(status_text)),
                            )
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_2()
                                    .text_sm()
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_color(muted)
                                            .child(SharedString::from(row.host.clone())),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .child(SharedString::from(row.path.clone())),
                                    ),
                            )
                            .child(
                                div()
                                    .w_16()
                                    .flex_none()
                                    .text_xs()
                                    .text_right()
                                    .text_color(muted)
                                    .child(SharedString::from(
                                        row.duration_ms.map(fmt_ms).unwrap_or_default(),
                                    )),
                            ),
                    )
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.scroll)
        .size_full();
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .p_2()
            .child(list)
            .child(Scrollbar::vertical(&self.scroll))
            .into_any_element()
    }

    fn render_detail(&self, e: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let mono = cx.theme().mono_font_family.clone();
        // 跨元素拖选按这个序号拼接文本，所以每段文字取一个递增序号
        let order = Cell::new(0u64);
        let text = |id: &'static str, value: String| {
            let n = order.get();
            order.set(n + 1);
            SelectableText::new((id, n as usize), value).document_order(n)
        };
        let section = |title: SharedString, body: Vec<AnyElement>| {
            v_flex()
                .gap_1()
                .px_4()
                .py_2()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(muted)
                        .child(title),
                )
                .children(body)
                .into_any_element()
        };
        let kv = |k: String, v: String| {
            let copy = v.clone();
            let key = text("dk", k);
            let val = text("dv", v);
            // 悬浮时出现的复制按钮：拖选不方便时（长 token、cookie）一键复制整个值
            let n = order.get() as usize;
            h_flex()
                .group("dkv")
                .gap_2()
                .items_start()
                .text_sm()
                .child(div().w(px(96.)).flex_none().text_color(muted).child(key))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(mono.clone())
                        .child(val),
                )
                .child(
                    div()
                        .flex_none()
                        .invisible()
                        .group_hover("dkv", |s| s.visible())
                        .child(Clipboard::new(("dc", n)).value(copy)),
                )
                .into_any_element()
        };
        let block = |body: String| {
            div()
                .text_xs()
                .font_family(mono.clone())
                .child(text("db", body))
                .into_any_element()
        };

        let req_headers = parse_headers(&e.req_headers);
        let res_headers = e
            .res_headers
            .as_deref()
            .map(parse_headers)
            .unwrap_or_default();
        let (status_text, status_col) = match e.status {
            Some(s) => (s.to_string(), status_color(s, cx)),
            None => (tr!("recorder.failed").to_string(), cx.theme().danger),
        };
        let method_col = Method::parse(&e.method)
            .map(|m| method_color(m, cx))
            .unwrap_or(muted);

        let mut sections: Vec<AnyElement> = Vec::new();

        // 标题：方法 + 状态 + 完整端点
        sections.push(
            v_flex()
                .gap_1()
                .px_4()
                .pt_3()
                .pb_2()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(method_col)
                                .child(SharedString::from(e.method.clone())),
                        )
                        .child(
                            div()
                                .text_color(status_col)
                                .child(SharedString::from(status_text)),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("record-copy-curl")
                                .small()
                                .label(tr!("recorder.menu_copy_curl"))
                                .on_click({
                                    let id = e.id;
                                    cx.listener(move |this, _, _, cx| this.copy_curl(id, cx))
                                }),
                        )
                        .child(
                            Button::new("record-load-test")
                                .small()
                                .primary()
                                .label(tr!("recorder.menu_load_test"))
                                .on_click({
                                    let id = e.id;
                                    cx.listener(move |this, _, _, cx| this.load_test(id, cx))
                                }),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .font_family(mono.clone())
                        .child(text("url", e.url.clone())),
                )
                .into_any_element(),
        );

        // 概览与耗时
        let mut overview = vec![kv(tr!("recorder.host").to_string(), e.host.clone())];
        overview.push(kv(tr!("recorder.path").to_string(), e.path.clone()));
        if let Some(q) = &e.query {
            overview.push(kv(tr!("recorder.query").to_string(), q.clone()));
        }
        overview.push(kv(
            tr!("recorder.type").to_string(),
            e.resource_type.clone(),
        ));
        if let Some(t) = e.started_ms {
            overview.push(kv(tr!("recorder.started").to_string(), fmt_utc(t)));
        }
        if let Some(v) = e.ttfb_ms {
            overview.push(kv(tr!("recorder.ttfb").to_string(), fmt_ms(v)));
        }
        if let Some(v) = e.duration_ms {
            overview.push(kv(tr!("recorder.duration").to_string(), fmt_ms(v)));
        }
        if let Some(v) = e.res_size {
            overview.push(kv(tr!("recorder.size").to_string(), fmt_bytes(v)));
        }
        if let Some(m) = &e.res_mime {
            overview.push(kv(tr!("recorder.mime").to_string(), m.clone()));
        }
        if let Some(err) = &e.error {
            overview.push(kv(tr!("recorder.error").to_string(), err.clone()));
        }
        sections.push(section(tr!("recorder.overview"), overview));

        // 主机：解析出的 IPv4 / IPv6、托管服务商、DNS 服务商（选中时查询，同一主机只查一次）
        let host_body: Vec<AnyElement> = match self.hosts.get(&e.host) {
            Some(HostState::Done(info)) => {
                host_rows(info).into_iter().map(|(k, v)| kv(k, v)).collect()
            }
            Some(HostState::Unknown) => {
                vec![block(tr!("tools.load_test.host_unknown").to_string())]
            }
            _ => vec![block(tr!("tools.load_test.host_loading").to_string())],
        };
        sections.push(section(tr!("recorder.host_info"), host_body));

        // 认证：常见的凭证头单独拎出来，不必在一长串头里找
        let auth: Vec<AnyElement> = req_headers
            .iter()
            .filter(|(k, _)| is_auth_header(k))
            .map(|(k, v)| kv(k.clone(), v.clone()))
            .collect();
        if !auth.is_empty() {
            sections.push(section(tr!("recorder.auth"), auth));
        }

        // Cookie：请求的 Cookie 头按分号拆开；响应的 Set-Cookie 一条一行
        let mut cookies: Vec<AnyElement> = Vec::new();
        for (k, v) in &req_headers {
            if k.eq_ignore_ascii_case("cookie") {
                for pair in v.split(';').map(str::trim).filter(|p| !p.is_empty()) {
                    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                    cookies.push(kv(name.to_string(), value.to_string()));
                }
            }
        }
        let mut set_cookies: Vec<AnyElement> = Vec::new();
        for (k, v) in &res_headers {
            if k.eq_ignore_ascii_case("set-cookie") {
                for line in v.lines().filter(|l| !l.trim().is_empty()) {
                    set_cookies.push(block(line.to_string()));
                }
            }
        }
        if !cookies.is_empty() {
            sections.push(section(tr!("recorder.cookies"), cookies));
        }
        if !set_cookies.is_empty() {
            sections.push(section(tr!("recorder.set_cookies"), set_cookies));
        }

        // 其余全部信息：协议、远端地址、TLS、分段耗时、发起者、重定向链…（摊平成 a.b.c → 值）
        if let Some(rows) = e
            .meta
            .as_deref()
            .map(flatten_meta)
            .filter(|r| !r.is_empty())
        {
            sections.push(section(
                tr!("recorder.details"),
                rows.into_iter().map(|(k, v)| kv(k, v)).collect(),
            ));
        }

        let header_rows = |h: &[(String, String)]| -> Vec<AnyElement> {
            h.iter()
                .filter(|(k, _)| !k.starts_with(':'))
                .map(|(k, v)| kv(k.clone(), v.clone()))
                .collect()
        };
        sections.push(section(
            tr!("recorder.req_headers"),
            header_rows(&req_headers),
        ));
        if let Some(body) = &e.post_data {
            let content_type = header(&req_headers, "content-type").unwrap_or_default();
            sections.push(section(
                tr!("recorder.req_body"),
                vec![block(body_text(body, false, &content_type))],
            ));
        }
        if !res_headers.is_empty() {
            sections.push(section(
                tr!("recorder.res_headers"),
                header_rows(&res_headers),
            ));
        }
        if let Some(body) = &e.res_body {
            let content_type = e.res_mime.clone().unwrap_or_default();
            sections.push(section(
                tr!("recorder.res_body"),
                vec![block(body_text(body, e.res_body_b64, &content_type))],
            ));
        }

        v_flex().pb_4().children(sections).into_any_element()
    }
}

#[cfg(test)]
impl RecordView {
    /// 测试用：直接灌入行与选中的详情，绕开数据库与后台任务。
    pub fn load_for_test(
        &mut self,
        entries: Vec<Entry>,
        detail: Option<Entry>,
        cx: &mut Context<Self>,
    ) {
        self.push(&entries, cx);
        if let Some(d) = detail {
            self.selected = Some(d.id);
            self.detail = Some(d);
        }
        cx.notify();
    }

    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub fn filter_for_test(&mut self, index: usize, cx: &mut Context<Self>) {
        self.set_filter(Filter::ALL[index], cx);
    }
}
