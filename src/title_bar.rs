use gpui::{Action, App, ElementId, Fill, Global, MouseButton, ReadGlobal, div, prelude::*, rems};

use crate::editor::EditorTheme;
use crate::window::{CloseWindow, MinimizeWindow, ZoomWindow};

pub struct FileInfo {
    pub path: std::path::PathBuf,
    pub dirty: bool,
}

impl Global for FileInfo {}

fn traffic_light(
    id: impl Into<ElementId>,
    bg: impl Into<Fill>,
    action: impl Action,
) -> impl IntoElement {
    div()
        .id(id)
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|style| style.opacity(0.7))
        .child(div().w(rems(1.0)).h(rems(1.0)).rounded_full().bg(bg))
        .on_click({
            let action = action.boxed_clone();
            move |_, window, cx| {
                window.dispatch_action(action.boxed_clone(), cx);
            }
        })
}

pub fn title_bar(theme: &EditorTheme, cx: &mut App) -> impl IntoElement {
    let file_info = FileInfo::global(cx);
    let file_name = file_info
        .path
        .file_name()
        .map(|n| n.display().to_string())
        .unwrap_or_else(|| "untitled".to_string());
    let title = if file_info.dirty {
        format!("* {}", file_name)
    } else {
        file_name
    };
    div()
        .id("title-bar")
        .w_full()
        .py(rems(0.5))
        .px(rems(1.0))
        .border_color(theme.selection)
        .border_b_1()
        .flex()
        .flex_row()
        .justify_between()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .relative()
                .on_mouse_down(MouseButton::Left, |_e, window, _| {
                    window.start_window_move();
                })
                // Invisible spacer to give height
                .child(
                    div()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .invisible()
                        .child(title.clone()),
                )
                // Actual text with ellipsis
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(title),
                ),
        )
        .child(
            div()
                .flex_shrink_0()
                .flex()
                .flex_row()
                .gap(rems(0.5))
                .child(traffic_light(
                    "minimize-button",
                    theme.orange,
                    MinimizeWindow,
                ))
                .child(traffic_light("maximize-button", theme.green, ZoomWindow))
                .child(traffic_light("quit-button", theme.red, CloseWindow)),
        )
}
