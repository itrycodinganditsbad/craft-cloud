//! The window's look, as a small kit the screens draw with: colour [`Tokens`] for the dark and the light appearance,
//! [`apply`] (fonts, visuals, spacing), and a few widgets: [`Button`] (outlined, primary or danger), [`icon_button`],
//! [`tag`], [`section_label`], [`card`], [`table_row`], [`headline`] and [`wordmark`].
//!
//! The look is hard edges, 1 px hairlines instead of shadows, Archivo for headlines, Inter for text and Geist Mono for
//! labels and buttons. Nothing outside this module hard-codes a colour: read [`Tokens::get`].
//!
//! The window calls [`ensure`] once per frame, so a context is set up before the first widget; [`apply`] sets it up
//! (the app calls it at start-up).

use std::sync::Arc;

use egui::epaint::text::VariationCoords;
use egui::{
    Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, FontTweak, Id, Response, RichText, Sense, Stroke, TextStyle, Ui,
    Visuals, Widget, WidgetInfo, WidgetType,
};

/// Colours of one appearance. The values are the Craft Cloud look's; light and dark differ only in these.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tokens {
    pub dark: bool,
    /// The window's and the cards' surface.
    pub bg: Color32,
    /// Wells: text fields, code, the table's header.
    pub sunken: Color32,
    /// Body text.
    pub ink: Color32,
    /// Headings.
    pub ink_strong: Color32,
    /// Secondary text.
    pub muted: Color32,
    /// Tertiary text and disabled things.
    pub faint: Color32,
    /// Hairlines.
    pub line: Color32,
    /// Outlined buttons and stronger rules.
    pub line_strong: Color32,
    /// The brand blue, for surfaces.
    pub accent: Color32,
    /// The brand blue, for text and icons.
    pub accent_ink: Color32,
    /// The primary button's fill, and the text on it.
    pub invert_bg: Color32,
    pub invert_fg: Color32,
    /// Good news (synced, online).
    pub good: Color32,
    /// Something to look at.
    pub warn: Color32,
    /// Something went wrong.
    pub bad: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    let [_, r, g, b] = rgb.to_be_bytes();
    Color32::from_rgb(r, g, b)
}

/// `rgb` at `alpha` (0..=255), not premultiplied.
const fn ink_alpha(rgb: u32, alpha: u8) -> Color32 {
    let [_, r, g, b] = rgb.to_be_bytes();
    Color32::from_rgba_unmultiplied_const(r, g, b, alpha)
}

impl Tokens {
    pub const DARK: Self = Self {
        dark: true,
        bg: hex(0x121316),
        sunken: hex(0x060607),
        ink: hex(0xf2f1ee),
        ink_strong: hex(0xffffff),
        muted: ink_alpha(0xf2f1ee, 153),
        faint: ink_alpha(0xf2f1ee, 97),
        line: ink_alpha(0xf2f1ee, 38),
        line_strong: ink_alpha(0xf2f1ee, 102),
        accent: hex(0x2d81ff),
        accent_ink: hex(0x74aaff),
        invert_bg: hex(0xffffff),
        invert_fg: hex(0x0b0b0c),
        good: hex(0x3fd6b8),
        warn: hex(0xffc14d),
        bad: hex(0xff6259),
    };

    pub const LIGHT: Self = Self {
        dark: false,
        bg: hex(0xf2f1ee),
        sunken: hex(0xe9e8e4),
        ink: hex(0x101014),
        ink_strong: hex(0x000000),
        muted: ink_alpha(0x101014, 158),
        faint: ink_alpha(0x101014, 107),
        line: ink_alpha(0x101014, 41),
        line_strong: ink_alpha(0x101014, 102),
        accent: hex(0x2d81ff),
        accent_ink: hex(0x1659c4),
        invert_bg: hex(0x101014),
        invert_fg: hex(0xf2f1ee),
        good: hex(0x0a7563),
        warn: hex(0x8a5800),
        bad: hex(0xb3261e),
    };

    /// The tokens of the appearance `ctx` is showing.
    pub fn get(ctx: &Context) -> Self {
        if ctx.theme() == egui::Theme::Dark { Self::DARK } else { Self::LIGHT }
    }

    /// A 1 px line in the hairline colour.
    pub fn hairline(&self) -> Stroke {
        Stroke::new(1.0, self.line)
    }

    /// The colour of a [`Tone`].
    pub fn tone(&self, tone: Tone) -> Color32 {
        match tone {
            Tone::Neutral => self.muted,
            Tone::Accent => self.accent_ink,
            Tone::Good => self.good,
            Tone::Warn => self.warn,
            Tone::Bad => self.bad,
        }
    }
}

/// What a [`tag`] says about its subject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Accent,
    Good,
    Warn,
    Bad,
}

/// The height of buttons and text fields.
pub const CONTROL_H: f32 = 22.0;
/// Size of labels, tags and buttons (Geist Mono).
const LABEL_SIZE: f32 = 10.0;

/// Inter 500.
pub fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

/// Archivo 620 at 118 % width: headlines and the wordmark.
pub fn display() -> FontFamily {
    FontFamily::Name("display".into())
}

/// Geist Mono 500: labels and tags.
pub fn mono() -> FontFamily {
    FontFamily::Name("mono".into())
}

/// Geist Mono 700: buttons.
pub fn mono_bold() -> FontFamily {
    FontFamily::Name("mono-bold".into())
}

/// A font registered under a name: its file, its variation coordinates (egui's variable-font axes) and its family.
type Face<'a> = (&'a str, &'static [u8], &'a [([u8; 4], f32)], FontFamily);

/// Registers the bundled fonts. egui 0.36 selects a variable font's axes per registered face (`FontTweak::coords`), so
/// each weight is its own face over the same file; egui's built-in fonts stay behind ours for symbols ours lack.
fn install_fonts(ctx: &Context) {
    const INTER: &[u8] = include_bytes!("../../assets/fonts/Inter-Variable.ttf");
    const ARCHIVO: &[u8] = include_bytes!("../../assets/fonts/Archivo-Variable.ttf");
    const GEIST_MONO: &[u8] = include_bytes!("../../assets/fonts/GeistMono-Variable.ttf");
    let mut fonts = FontDefinitions::default();
    let fallback: Vec<String> = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let faces: [Face<'_>; 6] = [
        ("Inter", INTER, &[(*b"wght", 400.0), (*b"opsz", 14.0)], FontFamily::Proportional),
        ("Inter-Medium", INTER, &[(*b"wght", 500.0), (*b"opsz", 14.0)], medium()),
        ("Archivo-Display", ARCHIVO, &[(*b"wght", 620.0), (*b"wdth", 118.0)], display()),
        ("GeistMono", GEIST_MONO, &[(*b"wght", 400.0)], FontFamily::Monospace),
        ("GeistMono-Medium", GEIST_MONO, &[(*b"wght", 500.0)], mono()),
        ("GeistMono-Bold", GEIST_MONO, &[(*b"wght", 700.0)], mono_bold()),
    ];
    for (name, bytes, coords, family) in faces {
        let tweak = FontTweak { coords: VariationCoords::new(coords.iter().copied()), ..Default::default() };
        fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_static(bytes).tweak(tweak)));
        let mut stack = vec![name.to_owned()];
        stack.extend(fallback.iter().cloned());
        fonts.families.insert(family, stack);
    }
    ctx.set_fonts(fonts);
}

fn visuals(t: &Tokens) -> Visuals {
    let mut v = if t.dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = t.bg;
    v.window_fill = t.bg;
    v.extreme_bg_color = t.sunken;
    v.text_edit_bg_color = Some(t.sunken);
    v.faint_bg_color = t.sunken;
    v.code_bg_color = t.sunken;
    v.window_stroke = Stroke::new(1.0, t.line_strong);
    v.window_corner_radius = CornerRadius::ZERO;
    v.menu_corner_radius = CornerRadius::ZERO;
    // Hairlines instead of shadows, except under popups and menus, which float.
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.popup_shadow = egui::epaint::Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(if t.dark { 140 } else { 36 }) };
    v.hyperlink_color = t.accent_ink;
    // Selected text keeps its colour on a translucent blue; the stroke is the focused text field's border.
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(t.accent.r(), t.accent.g(), t.accent.b(), 90);
    v.selection.stroke = Stroke::new(1.0, t.accent_ink);
    v.warn_fg_color = t.warn;
    v.error_fg_color = t.bad;
    v.widgets.noninteractive.bg_stroke = t.hairline();
    for (w, fill, stroke, fg) in [
        (&mut v.widgets.noninteractive, t.bg, t.line, t.ink),
        (&mut v.widgets.inactive, t.bg, t.line_strong, t.ink),
        (&mut v.widgets.hovered, t.sunken, t.ink, t.ink_strong),
        (&mut v.widgets.active, t.sunken, t.ink_strong, t.ink_strong),
        (&mut v.widgets.open, t.sunken, t.line_strong, t.ink_strong),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, stroke);
        w.corner_radius = CornerRadius::ZERO;
        w.fg_stroke = Stroke::new(1.0, fg);
        w.expansion = 0.0;
    }
    v
}

/// Sets `ctx` up: the fonts, both appearances' visuals, the type scale and the spacing. The appearance itself follows the
/// system (egui's default theme preference), which this leaves alone.
pub fn apply(ctx: &Context) {
    install_fonts(ctx);
    ctx.set_visuals_of(egui::Theme::Dark, visuals(&Tokens::DARK));
    ctx.set_visuals_of(egui::Theme::Light, visuals(&Tokens::LIGHT));
    ctx.all_styles_mut(|style| {
        style.spacing.button_padding = egui::vec2(8.0, 2.0);
        style.text_styles = [
            (TextStyle::Heading, FontId::new(19.0, display())),
            (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
        ]
        .into();
    });
    ctx.data_mut(|d| d.insert_temp(Id::new("craft-cloud-kit-applied"), true));
}

/// Whether `ctx` has been set up by [`apply`].
fn applied(ctx: &Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(Id::new("craft-cloud-kit-applied")).unwrap_or(false))
}

/// [`apply`] once per context, cheap to call every frame. Returns `false` in the frame it applied: egui takes new
/// fonts at the start of the next pass, so nothing may be laid out in the kit's families before then (the caller draws
/// nothing and a new pass is asked for). Call [`apply`] at start-up (the app does) to never see that frame.
pub fn ensure(ctx: &Context) -> bool {
    if applied(ctx) {
        return true;
    }
    apply(ctx);
    ctx.request_discard("fonts changed");
    ctx.request_repaint();
    false
}

/// Upper-case text in `family` with letter spacing, laid out in the placeholder colour (painted in the colour given at
/// paint time).
fn label_galley(ui: &Ui, text: &str, size: f32, family: FontFamily) -> Arc<egui::Galley> {
    let format =
        egui::TextFormat { font_id: FontId::new(size, family), extra_letter_spacing: size * 0.09, color: Color32::PLACEHOLDER, ..Default::default() };
    ui.painter().layout_job(egui::text::LayoutJob::single_section(text.to_uppercase(), format))
}

/// A label in upper-case mono. Screen readers get `text` as written, not upper-cased.
fn mono_label(ui: &mut Ui, text: &str, size: f32, family: FontFamily, color: Color32) -> Response {
    let galley = label_galley(ui, text, size, family);
    let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::hover());
    ui.painter().galley(rect.min, galley, color);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, text));
    response
}

/// The border that shows keyboard focus on a button.
fn focus_ring(ui: &Ui, rect: egui::Rect, t: &Tokens) {
    ui.painter().rect_stroke(rect, CornerRadius::ZERO, Stroke::new(2.0, t.accent_ink), egui::StrokeKind::Outside);
}

/// How a [`Button`] looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Outlined,
    Primary,
    Danger,
}

/// A button: outlined by default, or [`primary`](Self::primary) (inverted fill) or [`danger`](Self::danger). Its text is
/// drawn in upper-case mono, but screen readers and tests still get it as written.
#[derive(Clone, Debug)]
pub struct Button {
    text: String,
    kind: Kind,
    min_size: egui::Vec2,
    selected: bool,
}

impl Button {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), kind: Kind::Outlined, min_size: egui::vec2(0.0, CONTROL_H), selected: false }
    }

    /// The one main button of a place: inverted fill.
    pub fn primary(mut self) -> Self {
        self.kind = Kind::Primary;
        self
    }

    /// A button that deletes or ends something: the "bad" colour.
    pub fn danger(mut self) -> Self {
        self.kind = Kind::Danger;
        self
    }

    /// At least this size (the height is [`CONTROL_H`] otherwise).
    pub fn min_size(mut self, size: egui::Vec2) -> Self {
        self.min_size = size.max(self.min_size);
        self
    }

    /// Drawn as the chosen one of a group (inverted like [`primary`](Self::primary)).
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
}

impl Widget for Button {
    fn ui(self, ui: &mut Ui) -> Response {
        let t = Tokens::get(ui.ctx());
        let kind = if self.selected { Kind::Primary } else { self.kind };
        let enabled = ui.is_enabled();
        let galley = label_galley(ui, &self.text, LABEL_SIZE, mono_bold());
        let padding = ui.spacing().button_padding;
        let size = (galley.size() + 2.0 * padding).max(self.min_size);
        let (rect, response) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
        if ui.is_rect_visible(rect) {
            let hot = enabled && (response.hovered() || response.is_pointer_button_down_on());
            let (fill, border, text) = match (enabled, kind, hot) {
                (false, Kind::Primary, _) => (t.sunken, t.line, t.faint),
                (false, _, _) => (t.bg, t.line, t.faint),
                (true, Kind::Outlined, false) => (t.bg, t.line_strong, t.ink),
                (true, Kind::Outlined, true) => (t.sunken, t.ink, t.ink_strong),
                (true, Kind::Primary, false) => (t.invert_bg, t.invert_bg, t.invert_fg),
                (true, Kind::Primary, true) => (t.invert_bg.gamma_multiply(0.86), t.invert_bg, t.invert_fg),
                (true, Kind::Danger, false) => (t.bg, t.bad, t.bad),
                (true, Kind::Danger, true) => (t.sunken, t.bad, t.bad),
            };
            let painter = ui.painter();
            painter.rect(rect, CornerRadius::ZERO, fill, Stroke::new(1.0, border), egui::StrokeKind::Inside);
            painter.galley(rect.center() - galley.size() / 2.0, galley, text);
            if response.has_focus() {
                focus_ring(ui, rect, &t);
            }
        }
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, &self.text));
        response
    }
}

/// The painted pictures an [`icon_button`] can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Plus,
    Cross,
    Back,
    Dots,
}

/// A small square button with a painted icon. `label` is what screen readers and the tooltip say.
pub fn icon_button(ui: &mut Ui, label: &str, icon: Icon) -> Response {
    let t = Tokens::get(ui.ctx());
    let side = CONTROL_H;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), Sense::click());
    let enabled = ui.is_enabled();
    if ui.is_rect_visible(rect) {
        let (fill, border, color) = if !enabled {
            (t.bg, t.line, t.faint)
        } else if response.is_pointer_button_down_on() {
            (t.sunken, t.ink_strong, t.ink_strong)
        } else if response.hovered() {
            (t.sunken, t.ink, t.ink_strong)
        } else {
            (t.bg, t.line_strong, t.ink)
        };
        let painter = ui.painter();
        painter.rect(rect, CornerRadius::ZERO, fill, Stroke::new(1.0, border), egui::StrokeKind::Inside);
        if response.has_focus() {
            focus_ring(ui, rect, &t);
        }
        let painter = ui.painter();
        let c = rect.center();
        let stroke = Stroke::new(1.5, color);
        let r = 5.0;
        match icon {
            Icon::Plus => {
                painter.line_segment([c - egui::vec2(r, 0.0), c + egui::vec2(r, 0.0)], stroke);
                painter.line_segment([c - egui::vec2(0.0, r), c + egui::vec2(0.0, r)], stroke);
            }
            Icon::Cross => {
                painter.line_segment([c - egui::vec2(r, r), c + egui::vec2(r, r)], stroke);
                painter.line_segment([c - egui::vec2(r, -r), c + egui::vec2(r, -r)], stroke);
            }
            Icon::Back => {
                painter.line_segment([c + egui::vec2(r, 0.0), c - egui::vec2(r, 0.0)], stroke);
                painter.line_segment([c - egui::vec2(r, 0.0), c + egui::vec2(-r + 4.0, -4.0)], stroke);
                painter.line_segment([c - egui::vec2(r, 0.0), c + egui::vec2(-r + 4.0, 4.0)], stroke);
            }
            Icon::Dots => {
                for dx in [-5.0, 0.0, 5.0] {
                    painter.circle_filled(c + egui::vec2(dx, 0.0), 1.5, color);
                }
            }
        }
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    response.on_hover_text(label)
}

/// A small outlined badge in upper-case mono, coloured by `tone`: "Online", "Paused", "Admin".
pub fn tag(ui: &mut Ui, text: &str, tone: Tone) -> Response {
    let t = Tokens::get(ui.ctx());
    let color = t.tone(tone);
    let tint = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 20);
    let frame = egui::Frame::new().stroke(Stroke::new(1.0, color)).fill(tint).inner_margin(egui::Margin::symmetric(6, 2));
    frame.show(ui, |ui| mono_label(ui, text, LABEL_SIZE, mono(), color)).inner
}

/// The text of a section label: `01 / Title`.
pub fn numbered(number: usize, title: &str) -> String {
    format!("{number:02} / {title}")
}

/// A section's label: upper-case mono in the muted colour, `01 / Title` when given a number through [`numbered`].
pub fn section_label(ui: &mut Ui, text: &str) -> Response {
    let t = Tokens::get(ui.ctx());
    mono_label(ui, text, LABEL_SIZE, mono(), t.muted)
}

/// A line across the available width in the hairline colour.
pub fn hairline(ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, t.hairline());
}

/// A framed panel with a hairline border across the available width, and a header strip with `title` in a section label
/// over a hairline. `title` of `None` leaves the strip out.
pub fn card<R>(ui: &mut Ui, title: Option<&str>, add_contents: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    let t = Tokens::get(ui.ctx());
    let frame = egui::Frame::new().stroke(t.hairline()).fill(t.bg);
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 6.0;
        if let Some(title) = title {
            egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 7)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                section_label(ui, title);
            });
            hairline(ui);
        }
        egui::Frame::new()
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                add_contents(ui)
            })
            .inner
    })
}

/// One row of a table: the cells laid out left to right by `add_cells`, with a hairline under the row.
pub fn table_row<R>(ui: &mut Ui, add_cells: impl FnOnce(&mut Ui) -> R) -> R {
    ui.add_space(4.0);
    let inner = ui.horizontal(add_cells).inner;
    ui.add_space(4.0);
    hairline(ui);
    inner
}

/// The text of a headline, `size` points in the display face.
pub fn headline_text(text: &str, size: f32) -> RichText {
    RichText::new(text).family(display()).size(size).extra_letter_spacing(-0.02 * size)
}

/// A screen's headline, in the display face at the heading size.
pub fn headline(ui: &mut Ui, text: &str) -> Response {
    let t = Tokens::get(ui.ctx());
    ui.add(egui::Label::new(headline_text(text, 19.0).color(t.ink_strong)).selectable(false))
}

/// "Craft" followed by " Cloud" in the accent ink, in the display face.
pub fn wordmark(ui: &mut Ui, size: f32) -> Response {
    let t = Tokens::get(ui.ctx());
    let mut job = egui::text::LayoutJob::default();
    let format = |color| egui::TextFormat { font_id: FontId::new(size, display()), color, extra_letter_spacing: -0.03 * size, ..Default::default() };
    job.append("Craft", 0.0, format(t.ink_strong));
    job.append(" Cloud", 0.0, format(t.accent_ink));
    let response = ui.add(egui::Label::new(job).selectable(false));
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, "Craft Cloud"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_and_light_tokens_differ_where_they_should() {
        assert_ne!(Tokens::DARK.bg, Tokens::LIGHT.bg);
        assert_eq!(Tokens::DARK.accent, Tokens::LIGHT.accent);
        assert_ne!(Tokens::DARK.dark, Tokens::LIGHT.dark);
    }

    #[test]
    fn section_numbers_have_two_digits() {
        assert_eq!(numbered(1, "This computer"), "01 / This computer");
        assert_eq!(numbered(12, "x"), "12 / x");
    }

    #[test]
    fn tones_have_their_own_colours() {
        let t = Tokens::DARK;
        assert_eq!(t.tone(Tone::Good), t.good);
        assert_eq!(t.tone(Tone::Bad), t.bad);
        assert_ne!(t.tone(Tone::Warn), t.tone(Tone::Bad));
    }
}
