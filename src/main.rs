use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use gpui::{
    Bounds, Entity, FocusHandle, Focusable, KeyBinding, Point, Rems, Size, Window, WindowBounds,
    WindowDecorations, WindowOptions, div, prelude::*, px,
};
use writ::{
    buffer::Buffer,
    config::Config,
    demo::{DemoStep, DemoTiming, demo_script},
    editor::{Editor, EditorAction, EditorConfig, EditorTheme},
    git::{detect_github_context, parse_github_repo_string},
    github::GitHubClient,
    http,
    line::{CursorScreenPosition, HoveredRefScreenPosition},
    status_bar::StatusBarInfo,
    title_bar::FileInfo,
    window::{CloseWindow, MinimizeWindow, Quit, ZoomWindow, window_shadow},
};

/// Load a file and return its content.
fn load_file(file: &std::path::Path) -> String {
    match Buffer::from_file(file) {
        Ok((buffer, _)) => buffer.text(),
        Err(_) => String::new(),
    }
}

fn run_demo(editor: Entity<Editor>, cx: &mut gpui::App) {
    let script = demo_script();
    let timing = DemoTiming::default();

    cx.spawn(async move |cx| {
        let run = |cx: &gpui::AsyncApp, action: EditorAction| {
            cx.update(|cx| {
                if let Some(wh) = cx.windows().first().copied() {
                    let _ = cx.update_window(wh, |_, window, cx| {
                        editor.update(cx, |editor, cx| editor.execute(&action, window, cx));
                    });
                }
            });
        };

        cx.background_executor()
            .timer(Duration::from_millis(500))
            .await;

        for step in script {
            match step {
                DemoStep::Type(text) => {
                    for c in text.chars() {
                        run(cx, EditorAction::Type(c));
                        cx.background_executor().timer(timing.char_delay).await;
                    }
                }
                DemoStep::Wait(ms) => {
                    cx.background_executor()
                        .timer(Duration::from_millis(ms))
                        .await;
                }
                DemoStep::Action(action) => {
                    run(cx, action);
                    cx.background_executor().timer(timing.key_delay).await;
                }
            }
        }

        cx.background_executor()
            .timer(Duration::from_millis(500))
            .await;
        cx.update(|cx| {
            if let Some(wh) = cx.windows().first().copied() {
                let _ = cx.update_window(wh, |_, _, cx| {
                    editor.update(cx, |editor, _| editor.set_input_blocked(false));
                });
            }
        });
    })
    .detach();
}

pub struct Root {
    focus_handle: FocusHandle,
    document_editor: Entity<Editor>,
    theme: EditorTheme,
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        window_shadow(self.theme.clone()).child(
            div()
                .id("root")
                .track_focus(&self.focus_handle)
                .on_action(|CloseWindow, window, _| {
                    window.remove_window();
                })
                .on_action(|MinimizeWindow, window, _| {
                    window.minimize_window();
                })
                .on_action(|ZoomWindow, window, _| {
                    window.zoom_window();
                })
                .on_action(|Quit, _, cx| {
                    cx.quit();
                })
                .flex()
                .flex_col()
                .size_full()
                .overflow_hidden()
                .child(self.document_editor.clone()),
        )
    }
}

impl Focusable for Root {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn main() {
    // Install rustls crypto provider (required for TLS/HTTPS)
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = Config::parse()
        .validate()
        .expect("Failed to validate config");

    let demo_mode = config.demo;
    let file_path = config
        .file
        .clone()
        .unwrap_or_else(|| PathBuf::from("demo.md"));
    let content = if demo_mode {
        String::new()
    } else {
        load_file(&file_path)
    };

    let app = gpui_platform::application().with_http_client(http::Client::new());

    app.run(move |cx| {
        cx.set_global(FileInfo {
            path: file_path.clone(),
            dirty: false,
        });
        cx.set_global(StatusBarInfo::default());
        cx.set_global(EditorTheme::default());
        cx.set_global(CursorScreenPosition::default());
        cx.set_global(HoveredRefScreenPosition::default());
        cx.set_global(config);
        cx.bind_keys([
            KeyBinding::new("ctrl-w", CloseWindow, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        cx.on_window_closed(|cx, _window_id| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        cx.spawn(async move |cx| {
            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::new(0.0.into(), 0.0.into()),
                    size: Size::new(600.0.into(), 600.0.into()),
                })),
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            };

            cx.open_window(window_options, |window, cx| {
                // Create editor config from CLI config
                let cli_config = cx.global::<Config>();
                let theme = EditorTheme::dracula();
                let editor_config = EditorConfig {
                    theme: theme.clone(),
                    text_font: cli_config.text_font.clone(),
                    code_font: cli_config.code_font.clone(),
                    base_path: file_path.parent().map(|p| p.to_path_buf()),
                    padding_x: Rems(2.0),
                    padding_top: Rems(1.6),
                    padding_bottom: Rems(4.8),
                    line_height: Rems(1.6),
                    max_line_width: Some(px(800.0)),
                };

                // Extract config before borrowing cx mutably
                let github_repo = cli_config.github_repo.clone();
                let github_token = cli_config.github_token.clone();

                // Create the document editor
                let document_editor = cx.new(|cx| Editor::with_config(&content, editor_config, cx));

                // Set up GitHub context for autolink detection
                // Priority: CLI arg/env var > auto-detect from .git/config
                let github_context = github_repo
                    .as_ref()
                    .and_then(|s| parse_github_repo_string(s))
                    .or_else(|| detect_github_context(&file_path));

                if let Some(ctx) = github_context {
                    eprintln!("[writ] GitHub context: {}/{}", ctx.owner, ctx.repo);
                    document_editor.update(cx, |editor, _cx| {
                        editor.set_github_context(ctx);
                    });
                } else {
                    eprintln!("[writ] No GitHub context detected");
                }

                // Set up GitHub client if token is available
                if let Some(token) = github_token {
                    eprintln!("[writ] GitHub token provided ({} chars)", token.len());
                    let client = GitHubClient::new(token);
                    document_editor.update(cx, |editor, _cx| {
                        editor.set_github_client(client);
                    });
                } else {
                    eprintln!("[writ] No GitHub token - refs won't be validated");
                }

                // Set up file watching for external changes
                let watch_path = file_path.clone();
                document_editor.update(cx, |editor, cx| {
                    editor.watch_file(watch_path, cx);
                });

                // Focus the document editor so it receives keyboard input
                document_editor.focus_handle(cx).focus(window, cx);

                // Start demo if in demo mode
                if demo_mode {
                    // Block user input during demo
                    document_editor.update(cx, |editor, _| {
                        editor.set_input_blocked(true);
                    });
                    run_demo(document_editor.clone(), cx);
                }

                cx.new(|cx| {
                    cx.observe_global::<FileInfo>(|_, cx| {
                        cx.notify();
                    })
                    .detach();

                    cx.observe_global::<StatusBarInfo>(|_, cx| {
                        cx.notify();
                    })
                    .detach();

                    Root {
                        focus_handle: cx.focus_handle(),
                        document_editor,
                        theme,
                    }
                })
            })
            .expect("Failed to open window");
        })
        .detach();
    });
}
