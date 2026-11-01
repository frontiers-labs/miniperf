use gpui::{App, Global, Hsla, Pixels, SharedString, WindowAppearance, px, rgb, rgba};

/// Semantic + visualization color tokens ported verbatim from the approved
/// prototype (`ui-prototype/src/index.css`); oklch sources noted per value.
#[derive(Clone)]
pub struct Theme {
    pub dark: bool,

    pub background: Hsla,
    pub foreground: Hsla,
    pub card: Hsla,
    pub card_foreground: Hsla,
    pub popover: Hsla,
    pub popover_foreground: Hsla,
    pub primary: Hsla,
    pub primary_foreground: Hsla,
    pub secondary: Hsla,
    pub secondary_foreground: Hsla,
    pub muted: Hsla,
    pub muted_foreground: Hsla,
    pub accent: Hsla,
    pub accent_foreground: Hsla,
    pub destructive: Hsla,
    pub destructive_foreground: Hsla,
    pub border: Hsla,
    pub input: Hsla,
    pub ring: Hsla,

    pub viz: VizTheme,

    pub font_ui: SharedString,
    pub font_mono: SharedString,
}

#[derive(Clone)]
pub struct VizTheme {
    pub surface: Hsla,
    pub grid: Hsla,
    pub axis: Hsla,
    pub ink: Hsla,
    pub ink_2: Hsla,
    pub muted: Hsla,
    pub series: [Hsla; 8],
    pub status_good: Hsla,
    pub status_warn: Hsla,
    pub status_serious: Hsla,
    pub status_critical: Hsla,
}

pub const RADIUS: f32 = 10.0;

pub const TITLE_BAR_HEIGHT: f32 = 40.0;
pub const STATUS_BAR_HEIGHT: f32 = 24.0;
pub const LANE_HEIGHT: f32 = 15.0;
pub const FLAME_ROW_HEIGHT: f32 = 18.0;
pub const TIMELINE_GUTTER: f32 = 92.0;
pub const SELECTION_PANEL_WIDTH: f32 = 300.0;

impl Theme {
    pub fn light() -> Self {
        Self {
            dark: false,
            background: rgb(0xffffff).into(),         // oklch(1 0 0)
            foreground: rgb(0x0a0a0a).into(),         // oklch(0.145 0 0)
            card: rgb(0xffffff).into(),               // oklch(1 0 0)
            card_foreground: rgb(0x0a0a0a).into(),    // oklch(0.145 0 0)
            popover: rgb(0xffffff).into(),            // oklch(1 0 0)
            popover_foreground: rgb(0x0a0a0a).into(), // oklch(0.145 0 0)
            primary: rgb(0x171717).into(),            // oklch(0.205 0 0)
            primary_foreground: rgb(0xfafafa).into(), // oklch(0.985 0 0)
            secondary: rgb(0xf5f5f5).into(),          // oklch(0.97 0 0)
            secondary_foreground: rgb(0x171717).into(), // oklch(0.205 0 0)
            muted: rgb(0xf5f5f5).into(),              // oklch(0.97 0 0)
            muted_foreground: rgb(0x737373).into(),   // oklch(0.556 0 0)
            accent: rgb(0xf5f5f5).into(),             // oklch(0.97 0 0)
            accent_foreground: rgb(0x171717).into(),  // oklch(0.205 0 0)
            destructive: rgb(0xe7000b).into(),        // oklch(0.577 0.245 27.325)
            destructive_foreground: rgb(0xffffff).into(),
            border: rgb(0xe5e5e5).into(), // oklch(0.922 0 0)
            input: rgb(0xe5e5e5).into(),  // oklch(0.922 0 0)
            ring: rgb(0xa1a1a1).into(),   // oklch(0.708 0 0)
            viz: VizTheme::light(),
            font_ui: "Geist".into(),
            font_mono: "Menlo".into(),
        }
    }

    pub fn dark() -> Self {
        Self {
            dark: true,
            background: rgb(0x0a0a0a).into(), // oklch(0.145 0 0)
            foreground: rgb(0xfafafa).into(), // oklch(0.985 0 0)
            card: rgb(0x171717).into(),       // oklch(0.205 0 0)
            card_foreground: rgb(0xfafafa).into(), // oklch(0.985 0 0)
            popover: rgb(0x171717).into(),    // oklch(0.205 0 0)
            popover_foreground: rgb(0xfafafa).into(), // oklch(0.985 0 0)
            primary: rgb(0xe5e5e5).into(),    // oklch(0.922 0 0)
            primary_foreground: rgb(0x171717).into(), // oklch(0.205 0 0)
            secondary: rgb(0x262626).into(),  // oklch(0.269 0 0)
            secondary_foreground: rgb(0xfafafa).into(), // oklch(0.985 0 0)
            muted: rgb(0x262626).into(),      // oklch(0.269 0 0)
            muted_foreground: rgb(0xa1a1a1).into(), // oklch(0.708 0 0)
            accent: rgb(0x262626).into(),     // oklch(0.269 0 0)
            accent_foreground: rgb(0xfafafa).into(), // oklch(0.985 0 0)
            destructive: rgb(0xff6467).into(), // oklch(0.704 0.191 22.216)
            destructive_foreground: rgb(0xffffff).into(),
            border: rgba(0xffffff1a).into(), // oklch(1 0 0 / 10%)
            input: rgba(0xffffff26).into(),  // oklch(1 0 0 / 15%)
            ring: rgb(0x737373).into(),      // oklch(0.556 0 0)
            viz: VizTheme::dark(),
            font_ui: "Geist".into(),
            font_mono: "Menlo".into(),
        }
    }

    pub fn from_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
        }
    }

    pub fn radius_sm(&self) -> Pixels {
        px(RADIUS * 0.6)
    }

    pub fn radius_md(&self) -> Pixels {
        px(RADIUS * 0.8)
    }

    pub fn radius_lg(&self) -> Pixels {
        px(RADIUS)
    }

    pub fn radius_xl(&self) -> Pixels {
        px(RADIUS * 1.4)
    }

    /// Focus-visible ring color: `ring` at 50% opacity.
    pub fn focus_ring(&self) -> Hsla {
        self.ring.opacity(0.5)
    }

    /// Hover wash used by ghost/outline widgets.
    pub fn hover_wash(&self) -> Hsla {
        self.accent
    }

    pub fn module_kind_color(&self, kind: ModuleKind) -> Hsla {
        match kind {
            ModuleKind::User => self.viz.series[0],
            ModuleKind::Library => self.viz.series[2],
            ModuleKind::Runtime => self.viz.series[6],
            ModuleKind::Kernel => self.viz.series[1],
        }
    }

    pub fn tma_color(&self, category: TmaCategory) -> Hsla {
        match category {
            TmaCategory::Retiring => self.viz.series[2],
            TmaCategory::BadSpeculation => self.viz.series[4],
            TmaCategory::FrontendBound => self.viz.series[3],
            TmaCategory::BackendBound => self.viz.series[1],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModuleKind {
    User,
    Library,
    Runtime,
    Kernel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TmaCategory {
    Retiring,
    BadSpeculation,
    FrontendBound,
    BackendBound,
}

impl VizTheme {
    fn light() -> Self {
        Self {
            surface: rgb(0xfcfcfb).into(),
            grid: rgb(0xe1e0d9).into(),
            axis: rgb(0xc3c2b7).into(),
            ink: rgb(0x0b0b0b).into(),
            ink_2: rgb(0x52514e).into(),
            muted: rgb(0x898781).into(),
            series: [
                rgb(0x2a78d6).into(),
                rgb(0xeb6834).into(),
                rgb(0x1baf7a).into(),
                rgb(0xeda100).into(),
                rgb(0xe87ba4).into(),
                rgb(0x008300).into(),
                rgb(0x4a3aa7).into(),
                rgb(0xe34948).into(),
            ],
            status_good: rgb(0x0ca30c).into(),
            status_warn: rgb(0xfab219).into(),
            status_serious: rgb(0xec835a).into(),
            status_critical: rgb(0xd03b3b).into(),
        }
    }

    fn dark() -> Self {
        Self {
            surface: rgb(0x1a1a19).into(),
            grid: rgb(0x2c2c2a).into(),
            axis: rgb(0x383835).into(),
            ink: rgb(0xffffff).into(),
            ink_2: rgb(0xc3c2b7).into(),
            muted: rgb(0x898781).into(),
            series: [
                rgb(0x3987e5).into(),
                rgb(0xd95926).into(),
                rgb(0x199e70).into(),
                rgb(0xc98500).into(),
                rgb(0xd55181).into(),
                rgb(0x008300).into(),
                rgb(0x9085e9).into(),
                rgb(0xe66767).into(),
            ],
            status_good: rgb(0x0ca30c).into(),
            status_warn: rgb(0xfab219).into(),
            status_serious: rgb(0xec835a).into(),
            status_critical: rgb(0xd03b3b).into(),
        }
    }
}

impl Global for Theme {}

pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

/// Linear interpolation between two colors in RGB space, `t` in 0..=1.
pub fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let a: gpui::Rgba = a.into();
    let b: gpui::Rgba = b.into();
    let t = t.clamp(0.0, 1.0);
    gpui::Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
    .into()
}
