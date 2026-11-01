//! shadcn-ported widget kit (plan §5.3). Kit surface not yet consumed by a
//! view is expected while the rebuild is in progress.
#![allow(dead_code)]

pub mod badge;
pub mod button;
pub mod card;
pub mod chip;
pub mod dialog;
pub mod dropdown;
pub mod empty_state;
pub mod icon;
pub mod kbd;
pub mod meter;
pub mod section;
pub mod segmented;
pub mod separator;
pub mod splitter;
pub mod stat_tile;
pub mod tab_bar;
pub mod table;
pub mod text_input;
pub mod theme;
pub mod tooltip;
pub mod viz_card;

pub use badge::{BadgeVariant, badge};
pub use button::{ButtonSize, ButtonVariant, button};
pub use card::{card, card_description, card_section, card_title};
pub use chip::chip;
pub use dialog::dialog;
pub use dropdown::{DropdownItem, dropdown_menu};
pub use empty_state::empty_state;
pub use icon::{Icon, icon};
pub use kbd::{kbd, kbd_group};
pub use meter::{meter, segment_bar};
pub use section::{collapsible_section, section_caption};
pub use segmented::segmented;
pub use separator::separator;
pub use splitter::Splitter;
pub use stat_tile::stat_tile;
pub use tab_bar::{TabItem, tab_bar};
pub use table::{Column, table_cell, table_header, table_header_sortable, table_row};
pub use text_input::{InputEvent, TextInput};
pub use theme::{ActiveTheme, ModuleKind, Theme, TmaCategory};
pub use tooltip::info_tooltip;
pub use viz_card::{viz_card, viz_panel};
