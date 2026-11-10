//! The window's look, as a small kit the screens draw with: colour [`Tokens`] for the dark and the light appearance,
//! [`apply`] (fonts, visuals, spacing), and a few widgets: [`Button`] (outlined, primary or danger), [`icon_button`],
//! [`tag`], [`badge`], [`text_edit`], [`tab`], [`segmented`], [`text_link`], [`section_label`], [`card`], [`table`] (and the
//! older [`table_row`]), [`joined_cells`], [`headline`] and [`wordmark`].
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

/// The height of buttons and text fields (the mockups' main, security and trash screens use 36 to 40; 40 is the default).
pub const CONTROL_H: f32 = 40.0;
/// The height of the large controls of the first run ([`Button::large`], [`text_edit_large`]).
pub const CONTROL_H_LARGE: f32 = 44.0;
/// The height of a tab of the page bar.
const TAB_H: f32 = 32.0;
/// The size of a page's title, and of the first run's.
pub const HEADING_SIZE: f32 = 30.0;
pub const HEADING_SIZE_LARGE: f32 = 34.0;
/// Size of labels, tabs and buttons (Geist Mono).
const LABEL_SIZE: f32 = 12.0;
/// Size of tags (Geist Mono).
const TAG_SIZE: f32 = 11.0;
/// The gap between a control's border and its keyboard focus ring.
const FOCUS_GAP: f32 = 2.0;

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
        style.spacing.button_padding = egui::vec2(16.0, 4.0);
        // Solid scroll bars take their own room instead of floating over the content's right edge.
        style.spacing.scroll = egui::style::ScrollStyle::solid();
        style.text_styles = [
            (TextStyle::Heading, FontId::new(HEADING_SIZE, display())),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
        ]
        .into();
    });
    ctx.data_mut(|d| d.insert_temp(Id::new("craft-cloud-kit-applied"), true));
}

/// What the window is cleared with before anything is drawn, as eframe's `App::clear_color` wants it (`visuals` is the
/// appearance being shown). eframe's default is a translucent dark grey (12, 12, 12, 180). egui's panels reserve a 1 px
/// gap for their separator line, and the line is drawn in the hairline colour, which is translucent: over eframe's default
/// clear colour it came out near-black in the light appearance. kittest's renderer does not go through eframe, which is why
/// the snapshots never showed it. Clearing with the panel fill makes the hairline blend over the page's own colour.
pub fn clear_color(visuals: &Visuals) -> [f32; 4] {
    visuals.panel_fill.to_normalized_gamma_f32()
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

/// The border that shows keyboard focus on a control: 2 px wide, [`FOCUS_GAP`] outside the control's own border, so it never
/// blends with a red or a strong border.
fn focus_ring(ui: &Ui, rect: egui::Rect, t: &Tokens) {
    ui.painter().rect_stroke(rect.expand(FOCUS_GAP), CornerRadius::ZERO, Stroke::new(2.0, t.accent_ink), egui::StrokeKind::Outside);
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

    /// The large variant of the first run: [`CONTROL_H_LARGE`] high.
    pub fn large(mut self) -> Self {
        self.min_size.y = self.min_size.y.max(CONTROL_H_LARGE);
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

/// A small outlined badge in upper-case mono, coloured by `tone`: "Online", "Paused", "Admin". It keeps its natural size
/// (about 20 px high, 3 x 6 px padding) wherever it is put: it is allocated at exactly that size, so a table cell, a
/// right-to-left layout or a wrapped row never stretches it, and a row's centring layout centres it vertically.
pub fn tag(ui: &mut Ui, text: &str, tone: Tone) -> Response {
    let t = Tokens::get(ui.ctx());
    let color = t.tone(tone);
    let tint = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 20);
    let galley = label_galley(ui, text, TAG_SIZE, mono());
    let size = galley.size() + egui::vec2(12.0, 6.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect(rect, CornerRadius::ZERO, tint, Stroke::new(1.0, color), egui::StrokeKind::Inside);
        painter.galley(rect.center() - galley.size() / 2.0, galley, color);
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, text));
    response
}

/// A tab of a page bar: upper-case mono in regular weight, muted until hovered, the chosen one inverted. Draws no border
/// (the keyboard focus ring aside); `selected` is only the look, the caller knows which page is shown. When the bar is
/// disabled the chosen tab stays readable, outlined in the ink colour, while the others go faint.
pub fn tab(ui: &mut Ui, text: &str, selected: bool) -> Response {
    let t = Tokens::get(ui.ctx());
    let enabled = ui.is_enabled();
    let galley = label_galley(ui, text, LABEL_SIZE, mono());
    let size = (galley.size() + egui::vec2(16.0, 12.0)).max(egui::vec2(0.0, TAB_H));
    let (rect, response) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let hot = enabled && (response.hovered() || response.is_pointer_button_down_on());
        let (fill, color, border) = match (selected, enabled, hot) {
            (true, true, _) => (t.invert_bg, t.invert_fg, Color32::TRANSPARENT),
            (true, false, _) => (Color32::TRANSPARENT, t.ink_strong, t.ink),
            (false, true, true) => (t.sunken, t.ink_strong, Color32::TRANSPARENT),
            (false, true, false) => (Color32::TRANSPARENT, t.muted, Color32::TRANSPARENT),
            (false, false, _) => (Color32::TRANSPARENT, t.faint, Color32::TRANSPARENT),
        };
        let painter = ui.painter();
        painter.rect(rect, CornerRadius::ZERO, fill, Stroke::new(1.0, border), egui::StrokeKind::Inside);
        painter.galley(rect.center() - galley.size() / 2.0, galley, color);
        if response.has_focus() {
            focus_ring(ui, rect, &t);
        }
    }
    response.widget_info(|| WidgetInfo::selected(WidgetType::Button, enabled, selected, text));
    response
}

/// A single-line text field as high as a [`Button`], its text centred. egui has no style for a field's height, so the
/// screens build theirs with this instead of `egui::TextEdit::singleline`.
pub fn text_edit(text: &mut dyn egui::TextBuffer) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text).min_size(egui::vec2(0.0, CONTROL_H)).vertical_align(egui::Align::Center)
}

/// Like [`text_edit`], [`CONTROL_H_LARGE`] high (the first run).
pub fn text_edit_large(text: &mut dyn egui::TextBuffer) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text).min_size(egui::vec2(0.0, CONTROL_H_LARGE)).vertical_align(egui::Align::Center)
}

/// The width a [`tab`] of `text` takes.
pub fn tab_width(ui: &Ui, text: &str) -> f32 {
    label_galley(ui, text, LABEL_SIZE, mono()).size().x + 16.0
}

/// A [`section_label`] on one line at most `max_width` wide: what doesn't fit is cut off with an ellipsis, and the whole
/// text is the tooltip.
pub fn section_label_fit(ui: &mut Ui, text: &str, max_width: f32) -> Response {
    let t = Tokens::get(ui.ctx());
    let format = egui::TextFormat {
        font_id: FontId::new(LABEL_SIZE, mono()),
        extra_letter_spacing: LABEL_SIZE * 0.09,
        color: Color32::PLACEHOLDER,
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::single_section(text.to_uppercase(), format);
    job.wrap = egui::text::TextWrapping { max_width: max_width.max(0.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let galley = ui.painter().layout_job(job);
    let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::hover());
    ui.painter().galley(rect.min, galley, t.muted);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, text));
    response.on_hover_text(text)
}

/// A small filled mark in upper-case mono, coloured by `tone`: the number of a section (`01`). Natural size, like [`tag`].
pub fn badge(ui: &mut Ui, text: &str, tone: Tone) -> Response {
    let t = Tokens::get(ui.ctx());
    let galley = label_galley(ui, text, LABEL_SIZE, mono_bold());
    let size = galley.size() + egui::vec2(14.0, 6.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::ZERO, t.tone(tone));
        painter.galley(rect.center() - galley.size() / 2.0, galley, t.invert_fg);
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, text));
    response
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

/// A link in upper-case mono in the accent ink (`Computers ↗`, `Sign out`), `size` points high: it turns to the ink
/// colour when hovered and shows the keyboard focus ring. It is a button for screen readers.
pub fn text_link(ui: &mut Ui, text: &str, size: f32) -> Response {
    let t = Tokens::get(ui.ctx());
    let enabled = ui.is_enabled();
    let galley = label_galley(ui, text, size, mono());
    let pad = egui::vec2(0.0, 4.0);
    let (rect, response) = ui.allocate_exact_size(galley.size() + 2.0 * pad, if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let color = if !enabled {
            t.faint
        } else if response.hovered() || response.is_pointer_button_down_on() {
            t.ink_strong
        } else {
            t.accent_ink
        };
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
        if response.has_focus() {
            focus_ring(ui, rect, &t);
        }
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, text));
    if enabled { response.on_hover_cursor(egui::CursorIcon::PointingHand) } else { response }
}

/// One choice of a [`segmented`] control.
#[derive(Clone, Copy, Debug)]
pub struct Segment<'a> {
    pub text: &'a str,
    /// A number shown dimmed after the text ("All 3").
    pub count: Option<usize>,
    pub enabled: bool,
}

impl<'a> Segment<'a> {
    pub fn new(text: &'a str) -> Self {
        Self { text, count: None, enabled: true }
    }

    pub fn count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// A segmented control: one 1 px frame with a divider between the segments and no gaps; the `selected` segment is
/// inverted, a disabled one is faint. Every segment takes keyboard focus (Tab, then Enter or Space) and shows the focus
/// ring; screen readers get a radio group called `label` of radio buttons with their selected state. Returns the index
/// of the segment clicked, if any (the caller changes `selected`).
pub fn segmented(ui: &mut Ui, label: &str, segments: &[Segment<'_>], selected: usize) -> Option<usize> {
    let t = Tokens::get(ui.ctx());
    let group_id = ui.make_persistent_id(("segmented", label));
    let galleys: Vec<Arc<egui::Galley>> = segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (fg, count) = segment_colors(&t, s.enabled, i == selected, false);
            let mut job = egui::text::LayoutJob::default();
            let format = |color| egui::TextFormat {
                font_id: FontId::new(LABEL_SIZE, mono()),
                extra_letter_spacing: LABEL_SIZE * 0.09,
                color,
                ..Default::default()
            };
            job.append(&s.text.to_uppercase(), 0.0, format(fg));
            if let Some(n) = s.count {
                job.append(&format!(" {n}"), 0.0, format(count));
            }
            ui.painter().layout_job(job)
        })
        .collect();
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + 28.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 1.0;
    let (frame, group) = ui.allocate_exact_size(egui::vec2(total, CONTROL_H), Sense::hover());
    group.widget_info(|| WidgetInfo::labeled(WidgetType::RadioGroup, true, label));
    let mut clicked = None;
    let mut focused = None;
    let mut x = frame.left() + 1.0;
    for (i, segment) in segments.iter().enumerate() {
        let (Some(width), Some(galley)) = (widths.get(i).copied(), galleys.get(i)) else { continue };
        let rect = egui::Rect::from_min_size(egui::pos2(x, frame.top() + 1.0), egui::vec2(width, CONTROL_H - 2.0));
        x += width;
        let enabled = segment.enabled && ui.is_enabled();
        let response = ui.interact(rect, group_id.with(i), if enabled { Sense::click() } else { Sense::hover() });
        response.widget_info(|| WidgetInfo::selected(WidgetType::RadioButton, enabled, i == selected, segment.text));
        if ui.is_rect_visible(rect) {
            let hot = enabled && (response.hovered() || response.is_pointer_button_down_on());
            let on = i == selected;
            let fill = match (on, hot) {
                (true, _) => t.invert_bg,
                (false, true) => t.sunken,
                (false, false) => Color32::TRANSPARENT,
            };
            let (fg, _) = segment_colors(&t, enabled, on, hot);
            let painter = ui.painter();
            painter.rect_filled(rect, CornerRadius::ZERO, fill);
            painter.galley(rect.center() - galley.size() / 2.0, galley.clone(), fg);
            if i + 1 < segments.len() {
                painter.vline(rect.right(), frame.y_range(), Stroke::new(1.0, t.line_strong));
            }
        }
        if response.has_focus() {
            focused = Some(rect);
        }
        if response.clicked() && enabled {
            clicked = Some(i);
        }
    }
    if ui.is_rect_visible(frame) {
        ui.painter().rect_stroke(frame, CornerRadius::ZERO, Stroke::new(1.0, t.line_strong), egui::StrokeKind::Inside);
        if let Some(rect) = focused {
            focus_ring(ui, rect, &t);
        }
    }
    clicked
}

/// The text colour of a segment, and of its count.
fn segment_colors(t: &Tokens, enabled: bool, on: bool, hot: bool) -> (Color32, Color32) {
    match (on, enabled, hot) {
        (true, true, _) => (t.invert_fg, t.invert_fg.gamma_multiply(0.7)),
        (true, false, _) | (false, false, _) => (t.faint, t.faint),
        (false, true, true) => (t.ink_strong, t.muted),
        (false, true, false) => (t.muted, t.faint),
    }
}

/// How a [`table`] column gets its width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Width {
    /// Exactly this wide.
    Fixed(f32),
    /// At least `min` wide; what the table has beyond its columns' minimums is shared between the flexible columns by
    /// `weight`.
    Flex { min: f32, weight: f32 },
}

/// A column of a [`table`].
#[derive(Clone, Copy, Debug)]
pub struct Column<'a> {
    pub title: &'a str,
    pub width: Width,
}

impl<'a> Column<'a> {
    pub fn fixed(title: &'a str, width: f32) -> Self {
        Self { title, width: Width::Fixed(width) }
    }

    pub fn flex(title: &'a str, min: f32) -> Self {
        Self { title, width: Width::Flex { min, weight: 1.0 } }
    }

    /// A flexible column that takes `weight` shares of the spare room (a plain [`flex`](Self::flex) takes one).
    pub fn weighted(mut self, weight: f32) -> Self {
        if let Width::Flex { min, .. } = self.width {
            self.width = Width::Flex { min, weight };
        }
        self
    }
}

/// The room around a table cell's content, left and right.
const CELL_PAD: f32 = 16.0;
/// The height of a table's header row.
const TABLE_HEADER_H: f32 = 40.0;

/// The widths of `columns` in a table `total` wide (never below the fixed widths plus the flexible minimums).
pub fn column_widths(columns: &[Column<'_>], total: f32) -> Vec<f32> {
    let base: f32 = columns
        .iter()
        .map(|c| match c.width {
            Width::Fixed(w) => w,
            Width::Flex { min, .. } => min,
        })
        .sum();
    let weights: f32 = columns.iter().map(|c| if let Width::Flex { weight, .. } = c.width { weight } else { 0.0 }).sum();
    let extra = (total - base).max(0.0);
    columns
        .iter()
        .map(|c| match c.width {
            Width::Fixed(w) => w,
            Width::Flex { min, weight } if weights > 0.0 => min + extra * weight / weights,
            Width::Flex { min, .. } => min,
        })
        .collect()
}

/// The least width of a table with `columns`.
pub fn table_min_width(columns: &[Column<'_>]) -> f32 {
    column_widths(columns, 0.0).iter().sum()
}

/// A table the mockups' way: a header row of upper-case mono labels and `rows` rows of `row_height`, a 1 px hairline
/// under the header and every row (no stripes), a hairline frame around, as wide as the space it is given. Columns are
/// fixed or flexible with a minimum; when the space is below their minimums the table scrolls sideways in a scroll area of
/// its own, with a solid bar. `cell(ui, row, column)` fills one cell: the cell's `ui` centres what it holds vertically and
/// is clipped to the cell; the last column is right-aligned for actions, so add its rightmost widget first. For a name
/// over a muted path use one [`egui::Label`] of two formatted lines, or [`truncated`] for one line that ends with "…"
/// (and is shown whole as its hover text).
pub fn table(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    columns: &[Column<'_>],
    rows: usize,
    row_height: f32,
    mut cell: impl FnMut(&mut Ui, usize, usize),
) {
    let t = Tokens::get(ui.ctx());
    let id = ui.id().with(("table", id_salt));
    let width = ui.available_width().max(table_min_width(columns));
    let widths = column_widths(columns, width);
    let height = TABLE_HEADER_H + rows as f32 * row_height;
    let last = columns.len().saturating_sub(1);
    let layout = |c: usize| {
        if c == last { egui::Layout::right_to_left(egui::Align::Center) } else { egui::Layout::left_to_right(egui::Align::Center) }
    };
    egui::ScrollArea::horizontal().id_salt(id.with("scroll")).auto_shrink([false, true]).show(ui, |ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
        let painter = ui.painter().clone();
        let mut x = rect.left();
        for (c, column) in columns.iter().enumerate() {
            let w = widths.get(c).copied().unwrap_or_default();
            let cell_rect = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(w, TABLE_HEADER_H));
            x += w;
            let builder = egui::UiBuilder::new().id_salt((id, "head", c)).max_rect(cell_rect.shrink2(egui::vec2(CELL_PAD, 0.0))).layout(layout(c));
            let mut child = ui.new_child(builder);
            child.set_clip_rect(cell_rect.intersect(ui.clip_rect()));
            if !column.title.is_empty() {
                section_label(&mut child, column.title);
            }
        }
        painter.hline(rect.x_range(), rect.top() + TABLE_HEADER_H - 0.5, t.hairline());
        for row in 0..rows {
            let top = rect.top() + TABLE_HEADER_H + row as f32 * row_height;
            let mut x = rect.left();
            for (c, w) in widths.iter().enumerate() {
                let cell_rect = egui::Rect::from_min_size(egui::pos2(x, top), egui::vec2(*w, row_height));
                x += w;
                let builder = egui::UiBuilder::new().id_salt((id, row, c)).max_rect(cell_rect.shrink2(egui::vec2(CELL_PAD, 0.0))).layout(layout(c));
                let mut child = ui.new_child(builder);
                child.set_clip_rect(cell_rect.intersect(ui.clip_rect()));
                child.spacing_mut().item_spacing.x = 10.0;
                cell(&mut child, row, c);
            }
            if row + 1 < rows {
                painter.hline(rect.x_range(), top + row_height - 0.5, t.hairline());
            }
        }
        painter.rect_stroke(rect, CornerRadius::ZERO, t.hairline(), egui::StrokeKind::Inside);
    });
}

/// One line of text that ends with "…" when it doesn't fit, and is shown whole as its hover text.
pub fn truncated(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> Response {
    ui.add(egui::Label::new(text).truncate().selectable(false))
}

/// One cell of [`joined_cells`]: the badge (`01`) and the label on the right of its header strip. An empty badge draws
/// none and puts the label at the left (see [`JoinedCell::plain`]).
#[derive(Clone, Copy, Debug)]
pub struct JoinedCell<'a> {
    pub badge: &'a str,
    pub tone: Tone,
    /// The mono label on the right of the header strip.
    pub label: &'a str,
    /// The label is a link (`Computers ↗`): the call returns whether it was clicked.
    pub link: bool,
}

impl<'a> JoinedCell<'a> {
    pub fn new(badge: &'a str, tone: Tone, label: &'a str) -> Self {
        Self { badge, tone, label, link: false }
    }

    /// A cell without the numbered badge: only the label, at the left of its header strip.
    pub fn plain(tone: Tone, label: &'a str) -> Self {
        Self::new("", tone, label)
    }

    pub fn link(mut self) -> Self {
        self.link = true;
        self
    }
}

/// Which part of a [`joined_cells`] cell is being drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// The cell's content, from the top.
    Body,
    /// What sits at the bottom of the cell whatever the body's height (the actions).
    Footer,
}

/// The height of a joined cell's header strip, the room above and below its body, and the gap before its footer.
const JOINED_HEADER_H: f32 = 44.0;
const JOINED_PAD_Y: f32 = 20.0;
const JOINED_PAD_X: f32 = 16.0;
const JOINED_GAP: f32 = 14.0;

/// How many columns `cells` cells get in `width`: one below `stack_below`, otherwise a row.
pub fn joined_columns(cells: usize, width: f32, stack_below: f32) -> usize {
    if width < stack_below { 1 } else { cells.max(1) }
}

/// N cells that share hairlines: no gaps, every cell of a row as high as the highest, each with a header strip (a filled
/// badge on the left, a mono label on the right), a left-aligned body, and a footer at the bottom of the cell. They stack
/// into one column when `ui` is narrower than `stack_below`.
///
/// `add(ui, i, part)` draws the [`Part::Body`] and then the [`Part::Footer`] of cell `i` (with nothing drawn the footer
/// takes no room). Heights are measured while drawing and kept for the next frame, so a change of content takes one more
/// pass (the call asks for it). Returns, per cell, whether its header label was clicked (see [`JoinedCell::link`]).
pub fn joined_cells(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    cells: &[JoinedCell<'_>],
    stack_below: f32,
    mut add: impl FnMut(&mut Ui, usize, Part),
) -> Vec<bool> {
    let t = Tokens::get(ui.ctx());
    let id = ui.id().with(("joined-cells", id_salt));
    let width = ui.available_width().floor();
    let columns = joined_columns(cells.len(), width, stack_below);
    let cell_w = (width / columns as f32).floor();
    let stored: Vec<[f32; 2]> = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    let mut measured = vec![[0.0_f32; 2]; cells.len()];
    let mut clicked = vec![false; cells.len()];
    let top = ui.cursor().top();
    let left = ui.cursor().left();
    let mut grid_bottom = top;
    // The rows share their hairline: no gap between them.
    let item_spacing = std::mem::replace(&mut ui.spacing_mut().item_spacing.y, 0.0);
    for (row, chunk) in cells.chunks(columns).enumerate() {
        let first = row * columns;
        let y = ui.cursor().top();
        let stored_h = (first..first + chunk.len()).map(|i| stored.get(i).map_or(0.0, |s| s[0])).fold(0.0, f32::max);
        for (k, cell) in chunk.iter().enumerate() {
            let i = first + k;
            let x = left + k as f32 * cell_w;
            let inner = egui::Rect::from_min_size(egui::pos2(x + JOINED_PAD_X, y), egui::vec2(cell_w - 2.0 * JOINED_PAD_X, JOINED_HEADER_H));
            let mut head = ui
                .new_child(egui::UiBuilder::new().id_salt((id, i, "badge")).max_rect(inner).layout(egui::Layout::left_to_right(egui::Align::Center)));
            if !cell.badge.is_empty() {
                badge(&mut head, cell.badge, cell.tone);
            }
            // With a badge the label sits on the right of the strip; without one it starts at the left, as a stat's label.
            let label_layout = if cell.badge.is_empty() {
                egui::Layout::left_to_right(egui::Align::Center)
            } else {
                egui::Layout::right_to_left(egui::Align::Center)
            };
            let mut label = ui.new_child(egui::UiBuilder::new().id_salt((id, i, "label")).max_rect(inner).layout(label_layout));
            if cell.link {
                if let Some(slot) = clicked.get_mut(i) {
                    *slot = text_link(&mut label, cell.label, LABEL_SIZE).clicked();
                }
            } else {
                section_label(&mut label, cell.label);
            }
            let body_rect = egui::Rect::from_min_size(
                egui::pos2(x + JOINED_PAD_X, y + JOINED_HEADER_H + JOINED_PAD_Y),
                egui::vec2(cell_w - 2.0 * JOINED_PAD_X, 10_000.0),
            );
            let mut body =
                ui.new_child(egui::UiBuilder::new().id_salt((id, i, "body")).max_rect(body_rect).layout(egui::Layout::top_down(egui::Align::Min)));
            body.spacing_mut().item_spacing.y = JOINED_GAP;
            add(&mut body, i, Part::Body);
            let body_bottom = body.min_rect().bottom().max(body_rect.top());
            let stored_footer = stored.get(i).map_or(0.0, |s| s[1]);
            let footer_top = (y + stored_h.max(0.0) - JOINED_PAD_Y - stored_footer).max(body_bottom + JOINED_GAP);
            let footer_rect = egui::Rect::from_min_size(egui::pos2(x + JOINED_PAD_X, footer_top), egui::vec2(cell_w - 2.0 * JOINED_PAD_X, 10_000.0));
            let mut footer = ui
                .new_child(egui::UiBuilder::new().id_salt((id, i, "footer")).max_rect(footer_rect).layout(egui::Layout::top_down(egui::Align::Min)));
            footer.spacing_mut().item_spacing.y = item_spacing;
            add(&mut footer, i, Part::Footer);
            let footer_h = footer.min_rect().height();
            // The cell's own height, whatever the row's: the body, the gap, the footer.
            let natural = body_bottom + if footer_h > 0.0 { JOINED_GAP + footer_h } else { 0.0 } + JOINED_PAD_Y - y;
            if let Some(slot) = measured.get_mut(i) {
                *slot = [natural, footer_h];
            }
        }
        let row_h = (first..first + chunk.len()).map(|i| measured.get(i).map_or(0.0, |m| m[0])).fold(JOINED_HEADER_H + 2.0 * JOINED_PAD_Y, f32::max);
        let painter = ui.painter();
        for k in 0..chunk.len() {
            let x = left + k as f32 * cell_w;
            painter.hline(x..=x + cell_w, y + JOINED_HEADER_H - 0.5, t.hairline());
            painter.vline(x + cell_w - 0.5, y..=y + row_h, t.hairline());
            painter.hline(x..=x + cell_w, y + row_h - 0.5, t.hairline());
        }
        grid_bottom = y + row_h;
        ui.allocate_space(egui::vec2(cell_w * columns as f32, row_h));
    }
    ui.spacing_mut().item_spacing.y = item_spacing;
    let bottom = grid_bottom;
    let right = left + cell_w * columns as f32;
    ui.painter().hline(left..=right, top + 0.5, t.hairline());
    ui.painter().vline(left + 0.5, top..=bottom, t.hairline());
    // Equal heights need the highest cell of a row known before the row is drawn: use last frame's, and draw once more
    // when this frame's differ.
    let changed = stored.len() != measured.len() || stored.iter().zip(&measured).any(|(a, b)| (a[0] - b[0]).abs() > 0.5 || (a[1] - b[1]).abs() > 0.5);
    if changed {
        ui.data_mut(|d| d.insert_temp(id, measured));
        ui.ctx().request_discard("joined cells measured");
        ui.ctx().request_repaint();
    }
    clicked
}

/// The text of a headline, `size` points in the display face.
pub fn headline_text(text: &str, size: f32) -> RichText {
    RichText::new(text).family(display()).size(size).extra_letter_spacing(-0.02 * size)
}

/// A screen's headline, in the display face at the heading size ([`HEADING_SIZE`]).
pub fn headline(ui: &mut Ui, text: &str) -> Response {
    headline_sized(ui, text, HEADING_SIZE)
}

/// The first run's headline ([`HEADING_SIZE_LARGE`]).
pub fn headline_large(ui: &mut Ui, text: &str) -> Response {
    headline_sized(ui, text, HEADING_SIZE_LARGE)
}

fn headline_sized(ui: &mut Ui, text: &str, size: f32) -> Response {
    let t = Tokens::get(ui.ctx());
    ui.add(egui::Label::new(headline_text(text, size).color(t.ink_strong)).selectable(false))
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
