use gpui::{App, Hsla, IntoElement, Pixels, RenderOnce, Styled, Svg, Window, px, svg};

use super::theme::ActiveTheme;

/// Lucide icon subset bundled under `assets/icons/`, mirroring the prototype.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Icon {
    ArrowDown,
    ArrowUp,
    ArrowUpDown,
    Boxes,
    ChartNoAxesColumnIncreasing,
    ChartLine,
    File,
    Folder,
    PanelLeft,
    RotateCcw,
    ZoomIn,
    ZoomOut,
    MoveLeft,
    MoveRight,
    ArrowLeft,
    ArrowRight,
    Check,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    ChevronsUpDown,
    Circle,
    CircleDot,
    Clock,
    Command,
    Cpu,
    Ellipsis,
    FileCode2,
    FileCode,
    Flame,
    FolderOpen,
    Gauge,
    GitFork,
    Grid3x3,
    Info,
    Layers,
    LayoutDashboard,
    LineChart,
    MemoryStick,
    Minus,
    Moon,
    Mountain,
    PanelRightClose,
    PanelRightOpen,
    Play,
    Plus,
    Search,
    Server,
    Square,
    Sun,
    Table2,
    Terminal,
    X,
}

impl Icon {
    fn path(self) -> &'static str {
        match self {
            Self::ArrowDown => "icons/arrow-down.svg",
            Self::ArrowUp => "icons/arrow-up.svg",
            Self::ArrowUpDown => "icons/arrow-up-down.svg",
            Self::Boxes => "icons/boxes.svg",
            Self::ChartNoAxesColumnIncreasing => "icons/chart-no-axes-column-increasing.svg",
            Self::ChartLine => "icons/chart-line.svg",
            Self::File => "icons/file.svg",
            Self::Folder => "icons/folder.svg",
            Self::PanelLeft => "icons/panel-left.svg",
            Self::RotateCcw => "icons/rotate-ccw.svg",
            Self::ZoomIn => "icons/zoom-in.svg",
            Self::ZoomOut => "icons/zoom-out.svg",
            Self::MoveLeft => "icons/move-left.svg",
            Self::MoveRight => "icons/move-right.svg",
            Self::ArrowLeft => "icons/arrow-left.svg",
            Self::ArrowRight => "icons/arrow-right.svg",
            Self::Check => "icons/check.svg",
            Self::ChevronDown => "icons/chevron-down.svg",
            Self::ChevronLeft => "icons/chevron-left.svg",
            Self::ChevronRight => "icons/chevron-right.svg",
            Self::ChevronUp => "icons/chevron-up.svg",
            Self::ChevronsUpDown => "icons/chevrons-up-down.svg",
            Self::Circle => "icons/circle.svg",
            Self::CircleDot => "icons/circle-dot.svg",
            Self::Clock => "icons/clock.svg",
            Self::Command => "icons/command.svg",
            Self::Cpu => "icons/cpu.svg",
            Self::Ellipsis => "icons/ellipsis.svg",
            Self::FileCode2 => "icons/file-code-2.svg",
            Self::FileCode => "icons/file-code.svg",
            Self::Flame => "icons/flame.svg",
            Self::FolderOpen => "icons/folder-open.svg",
            Self::Gauge => "icons/gauge.svg",
            Self::GitFork => "icons/git-fork.svg",
            Self::Grid3x3 => "icons/grid-3x3.svg",
            Self::Info => "icons/info.svg",
            Self::Layers => "icons/layers.svg",
            Self::LayoutDashboard => "icons/layout-dashboard.svg",
            Self::LineChart => "icons/line-chart.svg",
            Self::MemoryStick => "icons/memory-stick.svg",
            Self::Minus => "icons/minus.svg",
            Self::Moon => "icons/moon.svg",
            Self::Mountain => "icons/mountain.svg",
            Self::PanelRightClose => "icons/panel-right-close.svg",
            Self::PanelRightOpen => "icons/panel-right-open.svg",
            Self::Play => "icons/play.svg",
            Self::Plus => "icons/plus.svg",
            Self::Search => "icons/search.svg",
            Self::Server => "icons/server.svg",
            Self::Square => "icons/square.svg",
            Self::Sun => "icons/sun.svg",
            Self::Table2 => "icons/table-2.svg",
            Self::Terminal => "icons/terminal.svg",
            Self::X => "icons/x.svg",
        }
    }

    pub fn render(self, size: Pixels, color: Hsla) -> Svg {
        svg().path(self.path()).size(size).text_color(color)
    }
}

#[derive(IntoElement)]
pub struct IconElement {
    icon: Icon,
    size: Pixels,
    color: Option<Hsla>,
}

pub fn icon(icon: Icon) -> IconElement {
    IconElement {
        icon,
        size: px(16.0),
        color: None,
    }
}

impl IconElement {
    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }
}

impl RenderOnce for IconElement {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = self.color.unwrap_or(cx.theme().foreground);
        self.icon.render(self.size, color)
    }
}
