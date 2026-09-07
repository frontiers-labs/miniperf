mod charts;
mod gallery;
mod memory;
mod model;
mod profile;
mod profile_analysis;
mod recent;
mod roofline;
mod shell;
mod snapshot;
mod source;
mod sql;
mod ui;

use std::{borrow::Cow, path::PathBuf};

use anyhow::Result;
use clap::Parser;
use gpui::{
    App, Application, AssetSource, Bounds, SharedString, TitlebarOptions, WindowBounds, point,
    prelude::*, px, size,
};

#[derive(Parser)]
#[command(about = "GPU-accelerated viewer for mperf result directories")]
struct Cli {
    /// Directory containing info.json and perf.db.
    result_directory: Option<PathBuf>,

    /// Open the widget gallery (design reference for the UI kit).
    #[arg(long)]
    gallery: bool,

    /// Open the recording, visit every tab, then exit. Reports what the window
    /// showed and fails if it could not show it.
    #[arg(long)]
    tour: bool,
}

struct MperfAssets;

static STATIC_ASSETS: &[(&str, &[u8])] = &[
    (
        "fonts/geist-variable.ttf",
        include_bytes!("../assets/fonts/geist-variable.ttf"),
    ),
    (
        "icons/arrow-left.svg",
        include_bytes!("../assets/icons/arrow-left.svg"),
    ),
    (
        "icons/arrow-right.svg",
        include_bytes!("../assets/icons/arrow-right.svg"),
    ),
    (
        "icons/check.svg",
        include_bytes!("../assets/icons/check.svg"),
    ),
    (
        "icons/chevron-down.svg",
        include_bytes!("../assets/icons/chevron-down.svg"),
    ),
    (
        "icons/chevron-left.svg",
        include_bytes!("../assets/icons/chevron-left.svg"),
    ),
    (
        "icons/chevron-right.svg",
        include_bytes!("../assets/icons/chevron-right.svg"),
    ),
    (
        "icons/chevron-up.svg",
        include_bytes!("../assets/icons/chevron-up.svg"),
    ),
    (
        "icons/chevrons-up-down.svg",
        include_bytes!("../assets/icons/chevrons-up-down.svg"),
    ),
    (
        "icons/circle-dot.svg",
        include_bytes!("../assets/icons/circle-dot.svg"),
    ),
    (
        "icons/circle.svg",
        include_bytes!("../assets/icons/circle.svg"),
    ),
    (
        "icons/clock.svg",
        include_bytes!("../assets/icons/clock.svg"),
    ),
    (
        "icons/command.svg",
        include_bytes!("../assets/icons/command.svg"),
    ),
    ("icons/cpu.svg", include_bytes!("../assets/icons/cpu.svg")),
    (
        "icons/ellipsis.svg",
        include_bytes!("../assets/icons/ellipsis.svg"),
    ),
    (
        "icons/file-code-2.svg",
        include_bytes!("../assets/icons/file-code-2.svg"),
    ),
    (
        "icons/flame.svg",
        include_bytes!("../assets/icons/flame.svg"),
    ),
    (
        "icons/folder-open.svg",
        include_bytes!("../assets/icons/folder-open.svg"),
    ),
    (
        "icons/gauge.svg",
        include_bytes!("../assets/icons/gauge.svg"),
    ),
    (
        "icons/git-fork.svg",
        include_bytes!("../assets/icons/git-fork.svg"),
    ),
    (
        "icons/grid-3x3.svg",
        include_bytes!("../assets/icons/grid-3x3.svg"),
    ),
    ("icons/info.svg", include_bytes!("../assets/icons/info.svg")),
    (
        "icons/layers.svg",
        include_bytes!("../assets/icons/layers.svg"),
    ),
    (
        "icons/layout-dashboard.svg",
        include_bytes!("../assets/icons/layout-dashboard.svg"),
    ),
    (
        "icons/line-chart.svg",
        include_bytes!("../assets/icons/line-chart.svg"),
    ),
    (
        "icons/memory-stick.svg",
        include_bytes!("../assets/icons/memory-stick.svg"),
    ),
    (
        "icons/minus.svg",
        include_bytes!("../assets/icons/minus.svg"),
    ),
    ("icons/moon.svg", include_bytes!("../assets/icons/moon.svg")),
    (
        "icons/mountain.svg",
        include_bytes!("../assets/icons/mountain.svg"),
    ),
    (
        "icons/panel-right-close.svg",
        include_bytes!("../assets/icons/panel-right-close.svg"),
    ),
    (
        "icons/panel-right-open.svg",
        include_bytes!("../assets/icons/panel-right-open.svg"),
    ),
    ("icons/play.svg", include_bytes!("../assets/icons/play.svg")),
    ("icons/plus.svg", include_bytes!("../assets/icons/plus.svg")),
    (
        "icons/search.svg",
        include_bytes!("../assets/icons/search.svg"),
    ),
    (
        "icons/server.svg",
        include_bytes!("../assets/icons/server.svg"),
    ),
    (
        "icons/square.svg",
        include_bytes!("../assets/icons/square.svg"),
    ),
    ("icons/sun.svg", include_bytes!("../assets/icons/sun.svg")),
    (
        "icons/table-2.svg",
        include_bytes!("../assets/icons/table-2.svg"),
    ),
    (
        "icons/terminal.svg",
        include_bytes!("../assets/icons/terminal.svg"),
    ),
    ("icons/x.svg", include_bytes!("../assets/icons/x.svg")),
];

impl AssetSource for MperfAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = STATIC_ASSETS.iter().find(|(name, _)| *name == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        Ok(roofline::roofline_label_svg(path).map(Cow::Owned))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// How long the tour waits for the asynchronous load before giving up.
const TOUR_LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// How long each tab is left on screen so its derived data is built and drawn.
const TOUR_TAB_DWELL: std::time::Duration = std::time::Duration::from_millis(250);

/// Drives the real window through every tab and exits non-zero if it could not
/// display the recording.
///
/// This is the GUI's test surface, and it is deliberately the production path:
/// the same `ShellView` a user gets, loading through the same code, switching
/// tabs through the same function the click handler calls. Nothing here can
/// pass while the window itself is broken.
fn run_tour(handle: gpui::WindowHandle<shell::ShellView>, cx: &mut App) {
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        let deadline = std::time::Instant::now() + TOUR_LOAD_TIMEOUT;
        loop {
            let state = handle
                .update(cx, |view, _, _| {
                    (
                        view.load_error().map(str::to_owned),
                        view.has_session(),
                        view.view_names(),
                    )
                })
                .ok();
            match state {
                Some((Some(error), _, _)) => {
                    eprintln!("tour: the viewer failed to load the recording: {error}");
                    std::process::exit(1);
                }
                Some((None, true, views)) if !views.is_empty() => {
                    println!("tour: {} views: {}", views.len(), views.join(", "));
                    for (ix, name) in views.iter().enumerate() {
                        if handle
                            .update(cx, |view, _, cx| view.activate_tab(ix, cx))
                            .is_err()
                        {
                            eprintln!("tour: the window closed while visiting {name}");
                            std::process::exit(1);
                        }
                        cx.background_executor().timer(TOUR_TAB_DWELL).await;
                        if let Ok(Some(error)) =
                            handle.update(cx, |view, _, _| view.load_error().map(str::to_owned))
                        {
                            eprintln!("tour: {name} reported an error: {error}");
                            std::process::exit(1);
                        }
                        println!("tour: rendered {name}");
                    }
                    println!("tour: ok");
                    std::process::exit(0);
                }
                Some(_) => {}
                None => {
                    eprintln!("tour: the window closed before the recording loaded");
                    std::process::exit(1);
                }
            }
            if std::time::Instant::now() > deadline {
                eprintln!("tour: the recording did not load within 60s");
                std::process::exit(1);
            }
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
        }
    })
    .detach();
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let gallery = cli.gallery;
    let tour = cli.tour;
    let initial_directory = cli.result_directory;

    if tour && initial_directory.is_none() {
        anyhow::bail!("--tour needs a result directory");
    }

    Application::new()
        .with_assets(MperfAssets)
        .run(move |cx: &mut App| {
            let geist = STATIC_ASSETS
                .iter()
                .find(|(name, _)| *name == "fonts/geist-variable.ttf")
                .map(|(_, bytes)| Cow::Borrowed(*bytes))
                .expect("bundled Geist font");
            if let Err(error) = cx.text_system().add_fonts(vec![geist]) {
                eprintln!("failed to register bundled Geist font: {error}");
            }
            ui::text_input::init(cx);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            if !gallery {
                shell::init(cx);
            }

            let bounds = Bounds::centered(None, size(px(1240.0), px(800.0)), cx);
            let options = gpui::WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some(if gallery { "mperf gallery" } else { "miniperf" }.into()),
                    appears_transparent: !gallery,
                    traffic_light_position: Some(point(px(10.0), px(11.0))),
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.0), px(600.0))),
                ..Default::default()
            };
            if gallery {
                cx.open_window(options, |window, cx| {
                    cx.set_global(ui::Theme::from_appearance(window.appearance()));
                    cx.new(|cx| gallery::Gallery::new(window, cx))
                })
                .expect("failed to open gallery window");
            } else {
                let initial_directory = initial_directory.clone();
                let handle = cx
                    .open_window(options, |window, cx| {
                        cx.set_global(ui::Theme::from_appearance(window.appearance()));
                        cx.new(|cx| shell::ShellView::new(initial_directory, window, cx))
                    })
                    .expect("failed to open shell window");
                if tour {
                    run_tour(handle, cx);
                }
            }
            cx.activate(true);
        });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn result_directory_is_optional() {
        assert!(Cli::try_parse_from(["mperf-gui"]).is_ok());
    }
}
