//! 压测的主区：目标请求、进度条、实时统计（已发 / 完成 / 失败 / 在途 / 耗时 / 吞吐）、
//! 状态码分布、失败原因、延迟分位数，以及目标主机所在的 IP 与服务商。数据全部读自
//! [`LoadSheet`]，它每 200 ms 刷新一次快照，所以跑的过程中这里就在动。

use gpui_kit::base::SelectableText;
use gpui_kit::component::{ActiveTheme, Sizable, button::Button, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::i18n::tr;
use crate::ui::load_sheet::{HostState, LoadSheet, Phase};
use crate::ui::record_fmt::{fmt_duration, host_rows, ms, summary_text};
use crate::ui::status_color;

pub struct LoadView {
    sheet: Entity<LoadSheet>,
    _sub: Subscription,
}

impl LoadView {
    pub fn new(sheet: &Entity<LoadSheet>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(sheet, |_, _, cx| cx.notify());
        Self {
            sheet: sheet.clone(),
            _sub: sub,
        }
    }
}

impl Render for LoadView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = self.sheet.read(cx);
        let muted = cx.theme().muted_foreground;
        if s.target().is_none() {
            return div()
                .id("load-view")
                .size_full()
                .p_6()
                .text_sm()
                .text_color(muted)
                .child(tr!("tools.load_test.empty"))
                .into_any_element();
        }
        let label = s.label(cx);
        let report = s.report().cloned().unwrap_or_default();
        let phase = s.phase().clone();
        let host_state = s.host().clone();
        let host_lines: Vec<(String, String)> = match &host_state {
            HostState::Done(info) => host_rows(info),
            _ => Vec::new(),
        };

        let (phase_text, phase_color) = match &phase {
            Phase::Idle => (tr!("tools.load_test.phase_idle"), muted),
            Phase::Running => (tr!("tools.load_test.running"), cx.theme().primary),
            Phase::Done => (tr!("tools.load_test.phase_done"), cx.theme().success),
            Phase::Cancelled => (tr!("tools.load_test.phase_cancelled"), cx.theme().warning),
            Phase::Failed(_) => (tr!("tools.load_test.phase_failed"), cx.theme().danger),
        };
        let frac = if report.planned == 0 {
            0.
        } else {
            (report.total as f32 / report.planned as f32).clamp(0., 1.)
        };
        let failed = report.failed();

        // 一张统计卡
        let card = |id: &'static str, label: SharedString, value: String, color: Option<Hsla>| {
            v_flex()
                .w(px(150.))
                .flex_none()
                .gap_1()
                .p_3()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .child(div().text_xs().text_color(muted).child(label))
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .font_family(cx.theme().mono_font_family.clone())
                        .when_some(color, |d, c| d.text_color(c))
                        .child(SelectableText::new(id, value)),
                )
        };
        // 一个带标题的区块，里面是 (名, 值) 行
        let block = |title: SharedString, rows: Vec<AnyElement>| {
            v_flex()
                .flex_1()
                .min_w(px(320.))
                .gap_1()
                .p_3()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(muted)
                        .child(title),
                )
                .children(rows)
        };
        let ix = std::cell::Cell::new(0usize);
        let kv = |k: String, v: String, color: Option<Hsla>| {
            let n = ix.get();
            ix.set(n + 1);
            h_flex()
                .gap_3()
                .items_start()
                .text_sm()
                .child(
                    div()
                        .w(px(84.))
                        .flex_none()
                        .text_color(muted)
                        .child(SharedString::from(k)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(cx.theme().mono_font_family.clone())
                        .when_some(color, |d, c| d.text_color(c))
                        .child(SelectableText::new(("load-kv", n), v)),
                )
                .into_any_element()
        };

        // 状态码分布：每行带一根按占比的条
        let max_count = report.statuses.values().copied().max().unwrap_or(1).max(1);
        let status_rows: Vec<AnyElement> = report
            .statuses
            .iter()
            .map(|(code, n)| {
                let col = status_color(*code, cx);
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_sm()
                    .child(
                        div()
                            .w(px(40.))
                            .flex_none()
                            .text_color(col)
                            .child(SharedString::from(code.to_string())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h_2()
                            .rounded_full()
                            .bg(cx.theme().muted)
                            .child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .bg(col)
                                    .w(relative(*n as f32 / max_count as f32)),
                            ),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .flex_none()
                            .text_right()
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(SharedString::from(n.to_string())),
                    )
                    .into_any_element()
            })
            .collect();
        let error_rows: Vec<AnyElement> = report
            .error_kinds
            .iter()
            .map(|(k, n)| kv(format!("×{n}"), k.clone(), Some(cx.theme().danger)))
            .collect();

        let latency = vec![
            kv("min".into(), ms(report.min), None),
            kv("p50".into(), ms(report.p50), None),
            kv("p90".into(), ms(report.p90), None),
            kv("p99".into(), ms(report.p99), None),
            kv("max".into(), ms(report.max), None),
        ];
        let network: Vec<AnyElement> = match &host_state {
            HostState::Loading => vec![
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(tr!("tools.load_test.host_loading"))
                    .into_any_element(),
            ],
            HostState::Unknown => vec![
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(tr!("tools.load_test.host_unknown"))
                    .into_any_element(),
            ],
            HostState::Done(_) => host_lines
                .iter()
                .map(|(k, v)| kv(k.clone(), v.clone(), None))
                .collect(),
        };

        let copy_text = summary_text(&label, &report, &host_lines);
        v_flex()
            .id("load-view")
            .size_full()
            .min_w_0()
            .overflow_y_scroll()
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(SelectableText::new("load-title", label.clone())),
                    )
                    .child(div().text_sm().text_color(phase_color).child(phase_text))
                    .child(
                        Button::new("load-copy")
                            .small()
                            .label(tr!("tools.load_test.copy_results"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()))
                            }),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .w_full()
                            .h_2()
                            .rounded_full()
                            .bg(cx.theme().muted)
                            .child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .bg(if failed > 0 {
                                        cx.theme().warning
                                    } else {
                                        cx.theme().primary
                                    })
                                    .w(relative(frac)),
                            ),
                    )
                    .child(div().text_xs().text_color(muted).child(tr!(
                        "tools.load_test.progress",
                        done = report.total,
                        planned = report.planned,
                        percent = (frac * 100.).round() as u32
                    ))),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_3()
                    .child(card(
                        "lc-sent",
                        tr!("tools.load_test.sent"),
                        report.sent.to_string(),
                        None,
                    ))
                    .child(card(
                        "lc-ok",
                        tr!("tools.load_test.completed"),
                        report.completed.to_string(),
                        None,
                    ))
                    .child(card(
                        "lc-failed",
                        tr!("tools.load_test.failed"),
                        failed.to_string(),
                        (failed > 0).then(|| cx.theme().danger),
                    ))
                    .child(card(
                        "lc-flight",
                        tr!("tools.load_test.in_flight"),
                        report.in_flight.to_string(),
                        None,
                    ))
                    .child(card(
                        "lc-elapsed",
                        tr!("tools.load_test.elapsed"),
                        fmt_duration(report.elapsed),
                        None,
                    ))
                    .child(card(
                        "lc-rps",
                        tr!("tools.load_test.throughput"),
                        format!("{:.1}/s", report.rps),
                        None,
                    )),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_start()
                    .gap_3()
                    .child(block(tr!("tools.load_test.statuses"), status_rows))
                    .child(block(tr!("tools.load_test.latency"), latency)),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .items_start()
                    .gap_3()
                    .when(!error_rows.is_empty(), |h| {
                        h.child(block(tr!("tools.load_test.errors"), error_rows))
                    })
                    .child(block(tr!("tools.load_test.network"), network)),
            )
            .into_any_element()
    }
}
