use std::sync::OnceLock;

use gpui::{
    AnyElement, App, Div, FontWeight, IntoElement as _, ParentElement as _, Styled as _, Window,
    div, px, rgb,
};
use gpui_component::{
    ActiveTheme as _, Icon, h_flex,
    text::{MarkdownExtensions, markdown_ast::Node},
};
use gpui_kit_assets::IconName;

pub(super) const RAIL_WIDTH: gpui::Pixels = px(3.);

struct Alert {
    marker: &'static str,
    title: &'static str,
    icon: IconName,
    light: u32,
    dark: u32,
}

const ALERTS: [Alert; 5] = [
    Alert {
        marker: "note",
        title: "Note",
        icon: IconName::Info,
        light: 0x365ac3,
        dark: 0x8ea2ff,
    },
    Alert {
        marker: "tip",
        title: "Tip",
        icon: IconName::Lightbulb,
        light: 0x18794e,
        dark: 0x65c994,
    },
    Alert {
        marker: "important",
        title: "Important",
        icon: IconName::CircleAlert,
        light: 0x7547bd,
        dark: 0xbd9cf6,
    },
    Alert {
        marker: "warning",
        title: "Warning",
        icon: IconName::TriangleAlert,
        light: 0x8c5c11,
        dark: 0xd3a95f,
    },
    Alert {
        marker: "caution",
        title: "Caution",
        icon: IconName::OctagonAlert,
        light: 0xb03c34,
        dark: 0xe3837a,
    },
];

pub(super) fn register(extensions: MarkdownExtensions) -> MarkdownExtensions {
    ALERTS.iter().fold(extensions, |extensions, alert| {
        extensions.titled_block_surface(alert.marker, |title, has_body, window, cx| {
            alert.render(title, has_body, window, cx)
        })
    })
}

pub(super) fn parser() -> &'static MarkdownExtensions {
    static PARSER: OnceLock<MarkdownExtensions> = OnceLock::new();
    PARSER.get_or_init(|| register(MarkdownExtensions::default().frontmatter()))
}

pub(crate) fn strip_markers(root: &mut Node, source: &str) {
    let Node::Root(root) = root else {
        return;
    };
    for child in &mut root.children {
        if let Node::Blockquote(quote) = child {
            parser().take_block_surface_marker(source, quote);
        }
    }
}

impl Alert {
    fn render(
        &self,
        title: Option<AnyElement>,
        has_body: bool,
        window: &mut Window,
        cx: &App,
    ) -> Div {
        let color = rgb(if cx.theme().mode.is_dark() {
            self.dark
        } else {
            self.light
        })
        .into();
        let font_size = window.text_style().font_size.to_pixels(window.rem_size());
        let mut title_row = h_flex()
            .items_start()
            .gap(font_size * 0.5)
            .text_color(color)
            .font_weight(FontWeight::SEMIBOLD)
            .child(
                h_flex()
                    .h(window.line_height())
                    .flex_shrink_0()
                    .child(Icon::new(self.icon).size(font_size)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(title.unwrap_or_else(|| div().child(self.title).into_any_element())),
            );
        if has_body {
            title_row = title_row.pb(px(4.));
        }
        div()
            .pl(px(2.) + font_size * 0.9)
            .content_left_rule(RAIL_WIDTH, color)
            .child(title_row)
    }
}
