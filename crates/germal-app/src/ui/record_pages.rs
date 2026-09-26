//! 录制的左侧栏：顶部是项目下拉（切换 / 新建项目），下面按域名与子域名组织的请求树。
//! 点「全部域名」、某个域名（含所有子域名）或某个具体主机，主区只看那一部分。

use gpui_kit::component::{
    ActiveTheme, IconName, Sizable,
    button::Button,
    h_flex,
    menu::{DropdownMenu as _, PopupMenuItem},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::i18n::tr;
use crate::ui::record_view::{RecordView, Scope};

pub struct RecordPages {
    view: Entity<RecordView>,
    _sub: Subscription,
}

impl RecordPages {
    pub fn new(view: &Entity<RecordView>, cx: &mut Context<Self>) -> Self {
        // 新行 / 选择 / 项目变化都要刷新树的计数与高亮
        let sub = cx.observe(view, |_, _, cx| cx.notify());
        Self {
            view: view.clone(),
            _sub: sub,
        }
    }
}

impl Render for RecordPages {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let (tree, scope, total, project_name) = {
            let v = self.view.read(cx);
            (
                v.domain_tree(),
                v.scope().clone(),
                v.total(),
                v.project_name(),
            )
        };
        let row = |id: (&'static str, usize),
                   target: Scope,
                   label: SharedString,
                   count: usize,
                   indent: bool,
                   bold: bool| {
            let on = scope == target;
            let view = self.view.clone();
            h_flex()
                .id(id)
                .h_8()
                .w_full()
                .px_2()
                .when(indent, |r| r.pl_6())
                .gap_2()
                .items_center()
                .rounded(cx.theme().radius)
                .when(on, |r| r.bg(cx.theme().list_active))
                .hover(|s| s.bg(cx.theme().list_hover))
                .aria_selected(on)
                .on_click(move |_, _, cx| {
                    let target = target.clone();
                    view.update(cx, |v, cx| v.select_scope(target, cx));
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .when(bold, |d| d.font_weight(FontWeight::SEMIBOLD))
                        .when(!bold, |d| d.text_color(muted))
                        .truncate()
                        .child(label),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(muted)
                        .child(SharedString::from(count.to_string())),
                )
        };

        let mut n = 0usize;
        let mut rows: Vec<AnyElement> = vec![
            row(
                ("record-scope", 0),
                Scope::All,
                tr!("recorder.all_domains"),
                total,
                false,
                true,
            )
            .into_any_element(),
        ];
        for node in &tree {
            n += 1;
            rows.push(
                row(
                    ("record-scope", n),
                    Scope::Domain(node.domain.clone()),
                    node.domain.clone().into(),
                    node.total,
                    false,
                    true,
                )
                .into_any_element(),
            );
            // 只有域名自己一个主机时不再重复列一行
            let only_self = node.hosts.len() == 1 && node.hosts[0].0 == node.domain;
            if !only_self {
                for (host, count) in &node.hosts {
                    n += 1;
                    rows.push(
                        row(
                            ("record-scope", n),
                            Scope::Host(host.clone()),
                            host.clone().into(),
                            *count,
                            true,
                            false,
                        )
                        .into_any_element(),
                    );
                }
            }
        }

        let view = self.view.clone();
        let project_button = Button::new("record-project")
            .outline()
            .small()
            .w_full()
            .label(project_name)
            .icon(IconName::Folder)
            .dropdown_caret(true)
            .tooltip(tr!("recorder.project"))
            .dropdown_menu(move |menu, _, cx| {
                let (projects, current) = {
                    let v = view.read(cx);
                    (v.projects().to_vec(), v.project())
                };
                let mut menu = menu;
                for p in projects {
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(SharedString::from(p.name.clone()))
                            .checked(p.id == current)
                            .on_click(move |_, _, cx| {
                                view.update(cx, |v, cx| v.switch_project(p.id, cx));
                            }),
                    );
                }
                let view = view.clone();
                menu.separator()
                    .item(PopupMenuItem::new(tr!("recorder.new_project")).on_click(
                        move |_, window, cx| {
                            view.update(cx, |v, cx| v.prompt_new_project(window, cx));
                        },
                    ))
            });

        v_flex()
            .id("record-pages")
            .flex_1()
            .min_h_0()
            .px_2()
            .pb_2()
            .gap_2()
            .child(project_button)
            .child(
                v_flex()
                    .id("record-domains")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap_0p5()
                    .children(rows)
                    .when(total == 0, |v| {
                        v.child(
                            div()
                                .px_2()
                                .pt_2()
                                .text_xs()
                                .text_color(muted)
                                .child(tr!("tools.recorder.hint")),
                        )
                    }),
            )
    }
}
