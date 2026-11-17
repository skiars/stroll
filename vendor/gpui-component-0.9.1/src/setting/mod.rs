mod fields;
mod group;
mod item;
mod page;
mod settings;

use gpui::{Pixels, px};

const CONTROL_FONT_SIZE: Pixels = px(13.5);
const CONTROL_LINE_HEIGHT: Pixels = px(18.);
const DESCRIPTION_FONT_SIZE: Pixels = px(12.);
const DESCRIPTION_LINE_HEIGHT: Pixels = px(17.);

pub use fields::*;
pub use group::*;
pub use item::*;
pub use page::*;
pub use settings::*;
