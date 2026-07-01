use gpui::{
    AnyElement, App, Bounds, BoxShadow, CursorStyle, Decorations, HitboxBehavior, Hsla,
    MouseButton, Pixels, Point, ResizeEdge, Size, Window, actions, canvas, div, point, prelude::*,
    px,
};

use crate::{editor::EditorTheme, status_bar::status_bar, title_bar::title_bar};

actions!(window, [CloseWindow, Quit, MinimizeWindow, ZoomWindow]);

#[derive(IntoElement)]
pub struct WindowShadow {
    children: Vec<AnyElement>,
    theme: EditorTheme,
}

impl WindowShadow {
    pub fn new(theme: EditorTheme) -> Self {
        Self {
            children: Vec::new(),
            theme,
        }
    }
}

pub fn window_shadow(theme: EditorTheme) -> WindowShadow {
    WindowShadow::new(theme)
}

impl ParentElement for WindowShadow {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for WindowShadow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let decorations = window.window_decorations();
        let rounding = px(10.0);
        let shadow_size = px(10.0);
        let border_size = px(1.0);
        let theme = &self.theme;
        window.set_client_inset(shadow_size);

        div()
            .id("window-backdrop")
            .text_color(theme.foreground)
            .map(|div| match decorations {
                Decorations::Server => div,
                Decorations::Client { tiling, .. } => div
                    .child(
                        canvas(
                            |bounds, window, _cx| {
                                window.insert_hitbox(
                                    Bounds::new(point(px(0.0), px(0.0)), bounds.size),
                                    HitboxBehavior::Normal,
                                )
                            },
                            move |bounds, hitbox, window, _cx| {
                                let mouse = window.mouse_position();
                                let size = bounds.size;
                                let Some(edge) = resize_edge(mouse, shadow_size, size) else {
                                    return;
                                };
                                window.set_cursor_style(
                                    match edge {
                                        ResizeEdge::Top | ResizeEdge::Bottom => {
                                            CursorStyle::ResizeUpDown
                                        }
                                        ResizeEdge::Left | ResizeEdge::Right => {
                                            CursorStyle::ResizeLeftRight
                                        }
                                        ResizeEdge::TopLeft | ResizeEdge::BottomRight => {
                                            CursorStyle::ResizeUpLeftDownRight
                                        }
                                        ResizeEdge::TopRight | ResizeEdge::BottomLeft => {
                                            CursorStyle::ResizeUpRightDownLeft
                                        }
                                    },
                                    &hitbox,
                                );
                            },
                        )
                        .size_full()
                        .absolute(),
                    )
                    .when(!(tiling.top || tiling.right), |div| {
                        div.rounded_tr(rounding)
                    })
                    .when(!(tiling.top || tiling.left), |div| div.rounded_tl(rounding))
                    .when(!(tiling.bottom || tiling.right), |div| {
                        div.rounded_br(rounding)
                    })
                    .when(!(tiling.bottom || tiling.left), |div| {
                        div.rounded_bl(rounding)
                    })
                    .when(!tiling.top, |div| div.pt(shadow_size))
                    .when(!tiling.bottom, |div| div.pb(shadow_size))
                    .when(!tiling.left, |div| div.pl(shadow_size))
                    .when(!tiling.right, |div| div.pr(shadow_size))
                    .on_mouse_move(move |e, window, _cx| {
                        // Only refresh when mouse is in the resize edge area
                        let size = window.viewport_size();
                        if resize_edge(e.position, shadow_size, size).is_some() {
                            window.refresh();
                        }
                    })
                    .on_mouse_down(MouseButton::Left, move |e, window, _cx| {
                        let size = window.viewport_size();
                        let pos = e.position;

                        if let Some(edge) = resize_edge(pos, shadow_size, size) {
                            window.start_window_resize(edge)
                        }
                    }),
            })
            .size_full()
            .child(
                div()
                    .cursor(CursorStyle::Arrow)
                    .map(|div| match decorations {
                        Decorations::Server => div,
                        Decorations::Client { tiling } => div
                            .border_color(theme.selection)
                            .when(!(tiling.top || tiling.right), |div| {
                                div.rounded_tr(rounding)
                            })
                            .when(!(tiling.top || tiling.left), |div| div.rounded_tl(rounding))
                            .when(!(tiling.bottom || tiling.right), |div| {
                                div.rounded_br(rounding)
                            })
                            .when(!(tiling.bottom || tiling.left), |div| {
                                div.rounded_bl(rounding)
                            })
                            .when(!tiling.top, |div| div.border_t(border_size))
                            .when(!tiling.bottom, |div| div.border_b(border_size))
                            .when(!tiling.left, |div| div.border_l(border_size))
                            .when(!tiling.right, |div| div.border_r(border_size))
                            .when(!tiling.is_tiled(), |div| {
                                div.shadow(vec![BoxShadow {
                                    color: Hsla {
                                        h: 0.,
                                        s: 0.,
                                        l: 0.,
                                        a: 0.4,
                                    },
                                    blur_radius: shadow_size / 2.,
                                    spread_radius: px(0.),
                                    offset: point(px(0.0), px(0.0)),
                                    inset: false,
                                }])
                            }),
                    })
                    .bg(theme.background)
                    .size_full()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(title_bar(theme, cx))
                    .child(
                        // Content area
                        div().flex_1().min_h_0().w_full().children(self.children),
                    )
                    .child(status_bar(cx)),
            )
    }
}

fn resize_edge(pos: Point<Pixels>, shadow_size: Pixels, size: Size<Pixels>) -> Option<ResizeEdge> {
    let edge = if pos.y < shadow_size && pos.x < shadow_size {
        ResizeEdge::TopLeft
    } else if pos.y < shadow_size && pos.x > size.width - shadow_size {
        ResizeEdge::TopRight
    } else if pos.y < shadow_size {
        ResizeEdge::Top
    } else if pos.y > size.height - shadow_size && pos.x < shadow_size {
        ResizeEdge::BottomLeft
    } else if pos.y > size.height - shadow_size && pos.x > size.width - shadow_size {
        ResizeEdge::BottomRight
    } else if pos.y > size.height - shadow_size {
        ResizeEdge::Bottom
    } else if pos.x < shadow_size {
        ResizeEdge::Left
    } else if pos.x > size.width - shadow_size {
        ResizeEdge::Right
    } else {
        return None;
    };
    Some(edge)
}
