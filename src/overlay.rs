//! Отрисовка панели.
//!
//! Панель рисуется вручную по сырому draw list, а не стоковыми виджетами
//! ImGui: нужны свой размер шрифта на элемент, разрядка у подписей и плита со
//! скруглением и волосяной рамкой - ничего из этого imgui-rs не выражает.
//!
//! Три правила отсюда не убирать, каждое стоило отдельного разбора:
//!  1. никакого `ui.get_window_draw_list()` / `DrawListMut` - imgui-rs паникует,
//!     если живы два инстанса одного draw list, а `panic = "abort"` в релизе
//!     превращает панику в краш игры. Только сырой `*mut ImDrawList`;
//!  2. никогда не хранить `FontId` - он не `Send`/`Sync`, а hudhook требует от
//!     render loop и того и другого. Шрифт достаётся по индексу атласа;
//!  3. вёрстка ручная: `ItemSpacing` в ноль, каждый элемент резервирует свою
//!     высоту через `ui.dummy()`. Поменял нарисованную высоту - меняй dummy.

use std::sync::atomic::{AtomicU32, Ordering};

use hudhook::imgui::sys as imgui_sys;
use hudhook::imgui::{Condition, Context, FontConfig, FontGlyphRanges, FontSource, StyleVar, Ui};

use crate::config::{BossCount, Config, Layout, PanelStyle};
use crate::i18n::t;
use crate::input;
use crate::stats::Snapshot;

type DrawList = *mut imgui_sys::ImDrawList;
type Font = *const imgui_sys::ImFont;

// Фиксированная отделка: базовый оттенок плиты (настраивается только
// прозрачность) и разрядка подписей.
const PANEL_BG_RGB: [f32; 3] = [0.04, 0.04, 0.055];
const PANEL_PAD_X: f32 = 14.0;
const PANEL_PAD_Y: f32 = 12.0;
/// Минимальный зазор между подписью слева и значением справа.
const MIN_GAP: f32 = 22.0;
/// Подпись счётчика боссов. Функция, а не константа: язык переключается в
/// настройках на живом кадре.
fn progress_label() -> &'static str {
    t("BOSSES")
}

// ---------------------------------------------------------------------------
// Примитивы
// ---------------------------------------------------------------------------

/// Общая прозрачность всей ручной отрисовки. `col32` подмешивает её в каждый
/// цвет, поэтому панель гаснет целиком, а не по частям. Ведёт её
/// `StreamHud::render` через `set_alpha`.
static HUD_ALPHA_BITS: AtomicU32 = AtomicU32::new(1.0f32.to_bits());

pub fn set_alpha(a: f32) {
    HUD_ALPHA_BITS.store(a.to_bits(), Ordering::Relaxed);
}

fn col32(color: [f32; 4]) -> u32 {
    let a = f32::from_bits(HUD_ALPHA_BITS.load(Ordering::Relaxed));
    hudhook::imgui::ImColor32::from([color[0], color[1], color[2], color[3] * a]).into()
}

/// Как `col32`, но мимо общей прозрачности панели: окно настроек не должно
/// выцветать вместе с HUD, который через него же и настраивают.
fn col32_solid(color: [f32; 4]) -> u32 {
    hudhook::imgui::ImColor32::from(color).into()
}

fn vec2(p: [f32; 2]) -> imgui_sys::ImVec2 {
    imgui_sys::ImVec2 { x: p[0], y: p[1] }
}

/// Draw list текущего окна, взятый сырым, чтобы `DrawListMut` не существовал
/// рядом с ним (см. правило 1 в шапке).
fn window_draw_list() -> DrawList {
    unsafe { imgui_sys::igGetWindowDrawList() }
}

/// Шрифт по позиции в атласе. Хранить `FontId` нельзя (правило 2).
fn font_at(index: usize) -> Font {
    unsafe {
        let atlas = (*imgui_sys::igGetIO()).Fonts;
        let fonts = &(*atlas).Fonts;
        if (index as i32) < fonts.Size {
            *fonts.Data.add(index)
        } else {
            std::ptr::null()
        }
    }
}

/// Текст заданным шрифтом и размером в пикселях - в безопасном API imgui-rs
/// переопределения на вызов нет. Нулевой `font` откатывается к дефолтному.
fn text_on(dl: DrawList, font: Font, pos: [f32; 2], color: [f32; 4], size_px: f32, text: &str) {
    let start = text.as_ptr().cast::<std::os::raw::c_char>();
    let end = unsafe { start.add(text.len()) };
    unsafe {
        imgui_sys::ImDrawList_AddText_FontPtr(
            dl,
            font,
            size_px,
            vec2(pos),
            col32(color),
            start,
            end,
            0.0,
            std::ptr::null(),
        );
    }
}

/// Два тёмных прохода со сдвигом (мягкий +2px, плотный +1px) - тяжелее
/// однопиксельной тени, ради вида Elden Ring и читаемости на светлой сцене.
fn text_shadowed(dl: DrawList, font: Font, pos: [f32; 2], color: [f32; 4], size_px: f32, text: &str) {
    // По целым пикселям: на дробной позиции глиф размазывается по двум
    // столбцам, и одна и та же подпись выходит то тоньше, то толще - вместе с
    // тенями, которые дробность наследуют.
    let pos = [pos[0].round(), pos[1].round()];
    text_on(dl, font, [pos[0] + 2.0, pos[1] + 2.0], [0.0, 0.0, 0.0, color[3] * 0.5], size_px, text);
    text_on(dl, font, [pos[0] + 1.0, pos[1] + 1.0], [0.0, 0.0, 0.0, color[3] * 0.85], size_px, text);
    text_on(dl, font, pos, color, size_px, text);
}

fn measure_text(font: Font, size_px: f32, text: &str) -> [f32; 2] {
    let font = if font.is_null() {
        unsafe { (*imgui_sys::igGetIO()).FontDefault as *const _ }
    } else {
        font
    };
    if font.is_null() {
        return [0.0, 0.0];
    }
    let start = text.as_ptr().cast::<std::os::raw::c_char>();
    let end = unsafe { start.add(text.len()) };
    let mut out = imgui_sys::ImVec2 { x: 0.0, y: 0.0 };
    unsafe {
        imgui_sys::ImFont_CalcTextSizeA(
            &mut out,
            font as *mut _,
            size_px,
            f32::MAX,
            0.0,
            start,
            end,
            std::ptr::null_mut(),
        );
    }
    [out.x, out.y]
}

/// Подъём шрифта для заданного кегля. Атлас запечён под один размер, при
/// рисовании другим ImGui масштабирует глифы линейно - метрики масштабируются
/// вместе с ними.
fn ascent(font: Font, size_px: f32) -> f32 {
    if font.is_null() {
        return size_px * 0.8;
    }
    let (a, baked) = unsafe { ((*font).Ascent, (*font).FontSize) };
    if baked <= 0.0 {
        return size_px * 0.8;
    }
    a * (size_px / baked)
}

/// Верхняя граница текста так, чтобы его базовая линия легла на `baseline`.
///
/// `ImDrawList_AddText` кладёт текст от ВЕРХНЕГО края, поэтому подпись 12px и
/// значение 22px, нарисованные с одного `y`, стоят на разных базовых линиях -
/// вот откуда бралось ощущение, что строка «горкой» (замечено живьём
/// 2026-08-18). Единственный способ выровнять текст разных кеглей - считать от
/// базовой линии.
fn top_for_baseline(font: Font, size_px: f32, baseline: f32) -> f32 {
    baseline - ascent(font, size_px)
}

/// Базовая линия строки, в которой встречаются кегли `sizes`: самый крупный
/// задаёт её положение, остальные подстраиваются.
fn row_baseline(font: Font, y: f32, sizes: &[f32]) -> f32 {
    y + sizes.iter().copied().fold(0.0f32, |m, s| m.max(ascent(font, s)))
}

/// Текст с разрядкой, возвращает нарисованную ширину. У ImGui нет
/// letter-spacing, а мелкая подпись капслоком без него читается не как
/// подпись, а как ужатый основной текст.
fn text_tracked(
    dl: DrawList,
    font: Font,
    pos: [f32; 2],
    color: [f32; 4],
    size_px: f32,
    tracking: f32,
    text: &str,
) -> f32 {
    let mut x = pos[0];
    let y = pos[1].round();
    for ch in text.chars() {
        let mut buf = [0u8; 4];
        let glyph = ch.encode_utf8(&mut buf);
        // Позиция глифа округляется, а `x` копится дробным: иначе ошибка
        // округления накапливалась бы по строке и разъезжалась бы разрядка.
        //
        // **Округление тут обязательно.** Ширины глифов дробные, поэтому
        // каждая следующая буква вставала на свою долю пикселя, и жирный
        // проход ниже накладывался на неё по-разному: одни буквы выходили
        // почти двойными, другие размытыми. На кириллице это тонуло в широких
        // литерах, а на латинице бросилось в глаза сразу - жалоба 2026-08-20
        // «шрифт HUD имеет разную толщину» была ровно про это.
        let gx = x.round();
        text_shadowed(dl, font, [gx, y], color, size_px, glyph);
        // Второй сплошной проход со сдвигом в полпикселя - подпись капслоком
        // на Palatino в 14-18px без него читается тонкой и хрупкой (отзыв
        // 2026-08-18, "БОССЫ слишком тонкое"). Дешевле нового файла шрифта:
        // отдельный bold-слот в атласе - это ещё один config-ключ и ещё одна
        // пересборка на каждую смену кегля.
        text_on(dl, font, [gx + 0.6, y], color, size_px, glyph);
        x += measure_text(font, size_px, glyph)[0] + tracking;
    }
    (x - tracking - pos[0]).max(0.0)
}

/// Ширина, которую займёт `text_tracked` - `measure_text` про разрядку не
/// знает, её надо добавить обратно.
fn tracked_width(font: Font, size_px: f32, tracking: f32, text: &str) -> f32 {
    let n = text.chars().count();
    if n == 0 {
        return 0.0;
    }
    measure_text(font, size_px, text)[0] + tracking * (n - 1) as f32
}

fn stroke_rect(dl: DrawList, min: [f32; 2], max: [f32; 2], color: [f32; 4], rounding: f32, thickness: f32) {
    unsafe {
        imgui_sys::ImDrawList_AddRect(
            dl,
            vec2(min),
            vec2(max),
            col32(color),
            rounding,
            imgui_sys::ImDrawFlags_RoundCornersAll as _,
            thickness,
        );
    }
}

fn fill_rect(dl: DrawList, min: [f32; 2], max: [f32; 2], color: [f32; 4], rounding: f32) {
    unsafe {
        imgui_sys::ImDrawList_AddRectFilled(
            dl,
            vec2(min),
            vec2(max),
            col32(color),
            rounding,
            imgui_sys::ImDrawFlags_RoundCornersAll as _,
        );
    }
}

fn gradient_h(dl: DrawList, min: [f32; 2], max: [f32; 2], left: [f32; 4], right: [f32; 4]) {
    if max[0] <= min[0] || max[1] <= min[1] {
        return;
    }
    unsafe {
        imgui_sys::ImDrawList_AddRectFilledMultiColor(
            dl,
            vec2(min),
            vec2(max),
            col32(left),
            col32(right),
            col32(right),
            col32(left),
        );
    }
}

pub(crate) fn with_alpha(color: [f32; 4], alpha: f32) -> [f32; 4] {
    [color[0], color[1], color[2], alpha]
}

/// Плитка окна настроек: заливка и тонкая рамка. Живёт здесь, а не в
/// `settings`, потому что сырой draw list наружу не выдаётся (правило 1 в
/// шапке модуля).
pub(crate) fn card_frame(min: [f32; 2], max: [f32; 2], fill: [f32; 4], border: [f32; 4], rounding: f32) {
    let dl = window_draw_list();
    fill_rect(dl, min, max, fill, rounding);
    stroke_rect(dl, min, max, border, rounding, 1.0);
}

/// Маркер аккордеона: треугольник вправо (закрыт) или вниз (открыт). Рисуем
/// сами, а не глифом: в атласе только Latin-1 и кириллица, и любая стрелка
/// вроде U+25B8 вышла бы на экран знаком вопроса.
pub(crate) fn caret(center: [f32; 2], size: f32, open: bool, color: [f32; 4]) {
    let (h, w) = (size * 0.5, size * 0.45);
    let (a, b, c) = match open {
        true => (
            [center[0] - h, center[1] - w],
            [center[0] + h, center[1] - w],
            [center[0], center[1] + w],
        ),
        false => (
            [center[0] - w, center[1] - h],
            [center[0] - w, center[1] + h],
            [center[0] + w, center[1]],
        ),
    };
    unsafe {
        imgui_sys::ImDrawList_AddTriangleFilled(
            window_draw_list(),
            a.into(),
            b.into(),
            c.into(),
            col32(color),
        );
    }
}

/// Линейка под заголовком плитки.
pub(crate) fn rule_line(pos: [f32; 2], w: f32, color: [f32; 4]) {
    let dl = window_draw_list();
    fill_rect(dl, pos, [pos[0] + w, pos[1] + 1.0], color, 0.0);
}

/// Открытая "скоба" вдоль одной грани скруглённого прямоугольника: прямая
/// часть, заворачивающаяся в четверть круга на концах, и обрыв - без боковых
/// стенок, в отличие от замкнутого `AddRect`.
///
/// Ради неё всё и заведено: золото обязано заходить в углы плиты тем же
/// радиусом, что и сама плита. Прямая линия, обрывающаяся до скругления,
/// читается как незаконченная рамка (жалоба 2026-08-20).
#[derive(Clone, Copy)]
enum Cap {
    Top,
    Bottom,
    Left,
    Right,
}

fn stroke_cap(dl: DrawList, min: [f32; 2], max: [f32; 2], color: [f32; 4], radius: f32, thickness: f32, cap: Cap) {
    let ([x0, y0], [x1, y1]) = (min, max);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    // Скругление больше половины стороны ImGui обрезает сам, но молча: две
    // дуги наехали бы друг на друга и путь свернулся бы в петлю.
    let r = radius.clamp(0.0, ((x1 - x0) * 0.5).min((y1 - y0) * 0.5));
    const PI: f32 = std::f32::consts::PI;
    // Углы ImGui: 0 - вправо, PI/2 - вниз (y растёт вниз), PI - влево.
    let arcs = match cap {
        Cap::Top => [([x0 + r, y0 + r], PI, 1.5 * PI), ([x1 - r, y0 + r], 1.5 * PI, 2.0 * PI)],
        Cap::Bottom => [([x0 + r, y1 - r], PI, 0.5 * PI), ([x1 - r, y1 - r], 0.5 * PI, 0.0)],
        Cap::Left => [([x0 + r, y1 - r], 0.5 * PI, PI), ([x0 + r, y0 + r], PI, 1.5 * PI)],
        Cap::Right => [([x1 - r, y0 + r], 1.5 * PI, 2.0 * PI), ([x1 - r, y1 - r], 0.0, 0.5 * PI)],
    };
    unsafe {
        imgui_sys::ImDrawList_PathClear(dl);
        for (center, a0, a1) in arcs {
            imgui_sys::ImDrawList_PathArcTo(dl, vec2(center), r, a0, a1, 0);
        }
        imgui_sys::ImDrawList_PathStroke(dl, col32(color), 0, thickness);
    }
}

/// Корпус панели. Три варианта (`Config::panel_style`), общие для игры и для
/// виджета в OBS:
///
/// - `Frame` - тёмная плитка с золотой скобой СВЕРХУ И СНИЗУ, без боковин;
/// - `Bare` - ничего, только текст со своей тенью;
/// - `Bar` / `BarRight` - тёмный фон и акцентная полоса сбоку, заходящая в
///   углы. Зеркальная нужна, когда панель стоит у правого края экрана.
///
/// Скругление и толщина линии - настройки (`panel_rounding`, `border_width`),
/// а не константы: сколько именно скруглять, зависит от кегля и от того, где
/// панель стоит, и подобрать это из кода нельзя.
///
/// Чего здесь больше нет (2026-08-20, запрос «минималистично, чтобы не лезло в
/// глаза»): блика по верхней грани. Он изображал освещённую плиту и тянул
/// взгляд на себя сильнее, чем цифры, ради которых панель существует.
/// Корпус, которым рисуется плита: у панели и у карточки покупки он свой.
///
/// Раньше `draw_panel_chrome` брал поля панели прямо из `Config`, и карточка
/// покупки ходила с её корпусом - настроить их порознь было нечем (запрос
/// 2026-08-23).
#[derive(Clone, Copy)]
pub struct Chrome {
    pub style: PanelStyle,
    pub fill: f32,
    pub border: f32,
    pub rounding: f32,
    pub width: f32,
}

impl Chrome {
    pub fn panel(c: &Config) -> Self {
        Self {
            style: c.panel_style,
            fill: c.panel_opacity,
            border: c.border_opacity,
            rounding: c.panel_rounding,
            width: c.border_width,
        }
    }

    pub fn toast(c: &Config) -> Self {
        Self {
            style: c.toast_style,
            fill: c.toast_opacity,
            border: c.toast_border_opacity,
            rounding: c.toast_rounding,
            width: c.toast_border_width,
        }
    }
}

fn draw_panel_chrome(dl: DrawList, pos: [f32; 2], size: [f32; 2], cfg: &Config) {
    draw_chrome(dl, pos, size, cfg, Chrome::panel(cfg))
}

fn draw_chrome(dl: DrawList, pos: [f32; 2], size: [f32; 2], cfg: &Config, ch: Chrome) {
    let [x, y] = pos;
    let [w, h] = size;
    // На первом кадре авторазмерное окно ещё не знает своего размера.
    if w < 8.0 || h < 8.0 {
        return;
    }
    if ch.style == PanelStyle::Bare {
        return;
    }
    // **ImGui обрезает края собственного окна.** В `Begin` он сужает clip rect
    // на `max(WindowPadding.x / 2, WindowBorderSize)` с каждого бока, чтобы
    // содержимое не липло к краю. У нас отступ большой (`s(14)`), и при
    // `ui_scale 1.7` это 11 пикселей: полоса слева шириной 5 рисовалась
    // целиком внутри обрезанной зоны и на экран не попадала вовсе - «где
    // полоса?», три круга проверок (2026-08-20). Дамп вершин этого не
    // показывает: вершины на месте, режет их clip rect уже при отрисовке.
    //
    // Поэтому корпус рисуется со СВОИМ clip rect по всему окну. Заодно
    // перестают срезаться углы плиты и края рамки.
    unsafe { imgui_sys::ImDrawList_PushClipRect(dl, vec2([x, y]), vec2([x + w, y + h]), false) };

    let r = cfg.s(ch.rounding).max(0.0);
    let bg = [PANEL_BG_RGB[0], PANEL_BG_RGB[1], PANEL_BG_RGB[2], ch.fill];
    fill_rect(dl, [x, y], [x + w, y + h], bg, r);

    let color = with_alpha(cfg.accent_color, ch.border);
    // Обводка идёт по СЕРЕДИНЕ линии, поэтому путь вжимается на полтолщины, а
    // радиус уменьшается на неё же: тогда внешний край скобы ложится ровно на
    // край плиты.
    let cap = |t: f32, which| {
        let half = t * 0.5;
        stroke_cap(dl, [x + half, y + half], [x + w - half, y + h - half], color, r - half, t, which);
    };
    // Пол в один ЭКРАННЫЙ пиксель: обводка сглаженная, и тоньше от неё
    // остаётся только полупрозрачный след - линия, которой «нет».
    let t = cfg.s(ch.width).max(1.0);
    match ch.style {
        // Скоба сверху и снизу, боковин нет: замкнутая рамка по периметру
        // читалась «оправой», а не краем (жалоба 2026-08-20).
        PanelStyle::Frame => {
            cap(t, Cap::Top);
            cap(t, Cap::Bottom);
        }
        PanelStyle::Bar => cap(t, Cap::Left),
        PanelStyle::BarRight => cap(t, Cap::Right),
        PanelStyle::Bare => {}
    }
    unsafe { imgui_sys::ImDrawList_PopClipRect(dl) };
}

/// Обрезка по ширине, а не по числу букв: у пропорционального шрифта это
/// разные вещи, а карточка держит фиксированную ширину.
fn clip_w(font: Font, size_px: f32, text: &str, max_w: f32) -> String {
    if measure_text(font, size_px, text)[0] <= max_w {
        return text.to_string();
    }
    let dots = measure_text(font, size_px, "...")[0];
    let mut end = 0;
    for (i, _) in text.char_indices() {
        if measure_text(font, size_px, &text[..i])[0] + dots > max_w {
            break;
        }
        end = i;
    }
    format!("{}...", text[..end].trim_end())
}

/// "17/206" -> ("17", "/206"). Убитых красим отдельно от знаменателя (см.
/// `draw_section_header`), формат всегда `{killed}/{total}` - `/` есть
/// гарантированно.
fn split_boss_counter(text: &str) -> (&str, &str) {
    let cut = text.find('/').unwrap_or(text.len());
    text.split_at(cut)
}

/// Заголовок секции: подпись с разрядкой и подчёркивание, гаснущее вправо.
/// Подчёркивание привязано к базовой линии подписи, а не к фиксированным
/// пикселям, поэтому едет вместе с кеглем и с `ui_scale`.
///
/// `trailing` - счётчик боссов `(killed, "/total")` двумя цветами: убитых
/// крупно и золотом, знаменатель - как обычное значение метрики ("смерти",
/// "уровень"), чтобы число убитых выделялось на их фоне, а не сливалось с
/// ним в одну гладкую строку (отзыв - в вебе так уже было, `.of` в
/// `web/index.html`, в оверлее нет).
/// Зазор вокруг полоски прогресса - один и тот же сверху и снизу.
fn section_gap(c: &Config) -> f32 {
    c.s(c.line_gap) * 2.0
}

fn draw_section_header(
    ui: &Ui,
    dl: DrawList,
    font: Font,
    label: &str,
    trailing: Option<(&str, &str)>,
    c: &Config,
    content_w: f32,
) {
    let [x0, y] = ui.cursor_screen_pos();
    // Без отступа под рельс: заголовок секции рельса не рисует, и левый
    // край подписи должен стоять там же, где WindowPadding кончается справа
    // у значения - иначе панель выглядит перекошенной (отзыв 2026-08-18).
    let x = x0;
    let big = trailing.map_or(c.label_size, |_| c.counter_size);
    let base = row_baseline(font, y, &[c.label_size, big]);
    // Подпись секции - цветом подписи, а не золотом (запрос 2026-08-20:
    // «золото только на главном»). Золото осталось у счётчика убитых и у
    // заливки полоски: когда его пять штук на панели, оно перестаёт быть
    // акцентом. В вебе так было с самого начала - заодно выводы сошлись.
    text_tracked(
        dl,
        font,
        [x, top_for_baseline(font, c.label_size, base)],
        c.label_color,
        c.label_size,
        c.s(c.tracking),
        label,
    );
    // Значение справа на той же строке, что и подпись: главная цифра панели
    // сидит в заголовке своей секции, а не теряется среди прочих строк.
    if let Some((killed, rest)) = trailing {
        let killed_w = measure_text(font, c.counter_size, killed)[0];
        let rest_w = measure_text(font, c.label_size, rest)[0];
        let start_x = x0 + content_w - killed_w - rest_w;
        text_shadowed(
            dl,
            font,
            [start_x, top_for_baseline(font, c.counter_size, base)],
            c.accent_color,
            c.counter_size,
            killed,
        );
        text_shadowed(
            dl,
            font,
            [start_x + killed_w, top_for_baseline(font, c.label_size, base)],
            c.label_color,
            c.label_size,
            rest,
        );
    }
    // Своей линейки у заголовка секции нет (прямой запрос 2026-08-27): она
    // переехала под строку ближайшего босса, где отделяет «куда идти» от «как
    // дела». Две горизонтали подряд читались как рябь.
    //
    // Зазор под заголовком - `SECTION_GAP`, ровно такой же, как под полоской
    // прогресса: разные числа сверху и снизу давали видимый перекос.
    // Высота самой строки плюс тот же зазор, что под полоской.
    ui.dummy([content_w, c.label_size.max(big) + section_gap(c)]);
}

/// Тонкая полоска прогресса: тёмный жёлоб, золотая заливка с бликом сверху.
/// Именно она превращает "18/235" из строчки таблицы во что-то, читаемое с
/// расстояния, на котором смотрят стрим.
fn draw_progress(dl: DrawList, x: f32, y: f32, w: f32, h: f32, ratio: f32, c: &Config) {
    let r = h * 0.5;
    fill_rect(dl, [x, y], [x + w, y + h], [0.0, 0.0, 0.0, 0.55], r);
    let fill_w = w * ratio.clamp(0.0, 1.0);
    if fill_w > 1.0 {
        // Плоская заливка: блик по верхней половине держался за прежнюю
        // «объёмную» плиту, а рядом с волосяной рамкой читался как лишний
        // блеск (тот же запрос про минимализм).
        fill_rect(dl, [x, y], [x + fill_w, y + h], c.accent_color, r);
    }
    stroke_rect(dl, [x, y], [x + w, y + h], with_alpha(c.accent_color, 0.35), r, 1.0);
}

/// Полоска здоровья союзника - в языке самой игры, а не панели.
///
/// Прямоугольная, красная, с «хвостом урона»: снятое только что остаётся
/// бледной полосой позади красной и стекает с задержкой (запрос 2026-09-08).
/// Скругления нет намеренно - у полосок игры его тоже нет, а ImGui на узком
/// прямоугольнике всё равно схлопывает радиус в ноль и уходит на сглаженный
/// путь, где тонкая заливка теряет по полпикселя с каждой стороны.
///
/// Цвета здесь литералами, а не из темы: это цитата чужого интерфейса, и
/// золото панели ей ни к чему.
fn draw_hp_bar(dl: DrawList, x: f32, y: f32, w: f32, h: f32, hp: f32, lag: f32, c: &Config) {
    const BACK: [f32; 4] = [0.05, 0.04, 0.03, 0.85];
    const FILL: [f32; 4] = [0.60, 0.11, 0.10, 1.0];
    const TAIL: [f32; 4] = [0.85, 0.78, 0.60, 0.95];

    let hp_w = w * hp.clamp(0.0, 1.0);
    let lag_w = w * lag.clamp(0.0, 1.0);
    fill_rect(dl, [x, y], [x + w, y + h], BACK, 0.0);
    // Хвост рисуется ПЕРВЫМ и на всю свою длину, красное ложится поверх:
    // так между ними не остаётся щели в полпикселя на дробных ширинах.
    if lag_w > hp_w + 0.5 {
        fill_rect(dl, [x, y], [x + lag_w, y + h], TAIL, 0.0);
    }
    if hp_w > 0.5 {
        fill_rect(dl, [x, y], [x + hp_w, y + h], FILL, 0.0);
    }
    stroke_rect(dl, [x, y], [x + w, y + h], with_alpha(c.label_color, 0.45), 0.0, 1.0);
}

/// Волосяная линия, самая яркая в середине и гаснущая к обоим краям. Две
/// половины, потому что `gradient_h` умеет только одно направление.
fn draw_divider(dl: DrawList, x: f32, y: f32, w: f32, color: [f32; 4]) {
    let mid = x + w * 0.5;
    let edge = with_alpha(color, 0.0);
    let center = with_alpha(color, 0.35);
    gradient_h(dl, [x, y], [mid, y + 1.0], edge, center);
    gradient_h(dl, [mid, y], [x + w, y + 1.0], center, edge);
}

// ---------------------------------------------------------------------------
// Шрифты
// ---------------------------------------------------------------------------

/// Состояние перетаскивания `xy_pad` между кадрами. У панели одна ручка
/// позиционирования, поэтому один слот (в `StreamHud`) на весь мод.
pub struct PadDrag {
    /// Чья это ручка. Слот перетаскивания один на окно, а ручек в одном
    /// разделе бывает две (панель и карточка покупки) - без владельца вторая
    /// каждый кадр обнуляла состояние первой, и первая переставала тянуться
    /// (жалоба 2026-08-21).
    owner: u64,
    /// Смещение объекта на момент захвата - от него отсчитывается ход.
    start: [f32; 2],
    /// Ось, к которой прижат Shift: 0 - свободно, 1 - X, 2 - Y.
    axis: u8,
    /// Точка экрана, к которой приколот курсор.
    anchor: [i32; 2],
    /// Курсор в конце прошлого кадра, уже после возврата на `anchor`. Смещение
    /// считается от неё, а не от самого `anchor`: если возврат не сработал
    /// (курсор кем-то зажат в другую область), разница с `anchor` не будет
    /// прибавляться каждый кадр сама по себе.
    last: [i32; 2],
    /// Сумма покадровых смещений курсора за всё перетаскивание.
    travel: [f32; 2],
}

/// Экранных пикселей смещения панели на один пиксель хода мыши. Меньше
/// единицы - тоньше управление. Портировано из elden как есть.
const XY_PAD_SENSITIVITY: f32 = 0.95;

/// Площадка выбора положения панели: миниатюра экрана, по которой таскаешь
/// точку. Возвращает `(изменилось, отпустили)` - второе нужно, чтобы писать в
/// `.ini` по отпусканию, а не каждый кадр перетаскивания.
///
/// Порт механики из elden (src/lib.rs:5143-5273): перетаскивание
/// относительное, курсор на время таскания приколот к точке захвата и скрыт,
/// а ход считается как сумма покадровых смещений. Иначе, уводя панель к краю,
/// курсор доезжает до границы экрана (или уходит на второй монитор) и
/// перетаскивание обрывается на середине - найдено живьём в elden. Shift
/// прижимает к той оси, по которой пошло первое заметное движение.
///
/// `width` приходит от плитки настроек: площадка это миниатюра экрана, и она
/// обязана занимать плитку целиком, а не сидеть в ней прежними 220 пикселями.
#[allow(clippy::too_many_arguments)]
/// Ключ владельца ручки. FNV-1a по строке id: хеш нужен только чтобы отличить
/// две ручки друг от друга, и тащить ради этого `DefaultHasher` незачем.
fn pad_key(id: &str) -> u64 {
    id.bytes().fold(0xcbf2_9ce4_8422_2325, |h: u64, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

pub fn xy_pad(
    ui: &Ui,
    id: &str,
    x: &mut f32,
    y: &mut f32,
    accent: [f32; 4],
    drag: &mut Option<PadDrag>,
    width: f32,
) -> (bool, bool) {
    let screen = ui.io().display_size;
    let (sw, sh) = (screen[0].max(1.0), screen[1].max(1.0));
    let pad_w = width.max(120.0);
    let pad_h = (pad_w * (sh / sw)).max(60.0);
    let origin = ui.cursor_screen_pos();

    ui.invisible_button(id, [pad_w, pad_h]);
    let active = ui.is_item_active();
    let released = ui.is_item_deactivated();

    if active {
        let grab = input::cursor_pos();
        // Чужое состояние не наследуем: у соседней ручки свой захват и своя
        // точка отсчёта.
        if drag.as_ref().is_some_and(|d| d.owner != pad_key(id)) {
            *drag = None;
        }
        let state = drag.get_or_insert_with(|| PadDrag {
            owner: pad_key(id),
            start: [*x, *y],
            axis: 0,
            anchor: grab,
            last: grab,
            travel: [0.0, 0.0],
        });
        // Ход за кадр, потом курсор обратно на место захвата - он никуда не
        // едет и не может уйти с экрана. Курсор рисует ImGui (`MouseDrawCursor`),
        // и `render` включает его заново каждый кадр, так что прятать его тут
        // безопасно: само вернётся, как только перетаскивание закончится.
        let now = input::cursor_pos();
        state.travel[0] += (now[0] - state.last[0]) as f32;
        state.travel[1] += (now[1] - state.last[1]) as f32;
        input::pin_cursor(state.anchor);
        state.last = input::cursor_pos();
        unsafe { (*imgui_sys::igGetIO()).MouseDrawCursor = false };

        let (start_val, dd) = (state.start, state.travel);
        let mut axis = state.axis;
        if ui.io().key_shift {
            if axis == 0 && dd[0].abs().max(dd[1].abs()) > 6.0 {
                axis = if dd[0].abs() >= dd[1].abs() { 1 } else { 2 };
                state.axis = axis;
            }
        } else if axis != 0 {
            axis = 0;
            state.axis = 0;
        }

        if axis != 2 {
            *x = (start_val[0] + dd[0] * XY_PAD_SENSITIVITY).clamp(0.0, sw);
        }
        if axis != 1 {
            *y = (start_val[1] + dd[1] * XY_PAD_SENSITIVITY).clamp(0.0, sh);
        }
    }
    // Сбрасываем только СВОЁ: сосед по разделу иначе стирал бы наш захват на
    // том же кадре, в котором он начался.
    if !active && drag.as_ref().is_some_and(|d| d.owner == pad_key(id)) {
        *drag = None;
    }

    let dl = window_draw_list();
    let max = [origin[0] + pad_w, origin[1] + pad_h];
    fill_rect(dl, origin, max, [0.0, 0.0, 0.0, 0.35], 4.0);
    stroke_rect(dl, origin, max, with_alpha(accent, 0.6), 4.0, 1.5);

    // Точка стоит там же, где встанет левый верхний угол панели.
    let px = origin[0] + (*x / sw).clamp(0.0, 1.0) * pad_w;
    let py = origin[1] + (*y / sh).clamp(0.0, 1.0) * pad_h;
    unsafe {
        imgui_sys::ImDrawList_AddCircleFilled(dl, vec2([px, py]), 4.5, col32_solid(accent), 12);
    }
    (active, released)
}

/// Кегль шрифта окна настроек. Намеренно НЕ в `Config`: раньше окно рисовалось
/// тем же шрифтом, что и панель (слот 0), и подстройка кеглей HUD заодно
/// растягивала само меню - правишь размер, а под тобой едет интерфейс, которым
/// правишь. Ровно на это наступили в elden 2026-07-28, здесь повторили
/// 2026-08-18.
const SETTINGS_FONT_PX: f32 = 19.0;

/// Атлас адресуется по позиции (`font_at`), поэтому слот добавляется
/// **всегда**: пропущенный сдвинул бы все последующие индексы, панель начала бы
/// рисоваться не тем шрифтом, а `font_at` за концом вернул бы null - краш в
/// любом замере текста. Встроенный битмап-шрифт ImGui держит нумерацию честной.
///
/// Слот 0 - панель, слот 1 - окно настроек.
pub fn load_fonts(ctx: &mut Context, cfg: &Config) {
    let bytes = std::fs::read(&cfg.font_path).ok();
    // Стрелки высоты (U+2191/U+2193) Palatino не содержит вовсе - живьём
    // 2026-08-27 они вышли знаками вопроса, хотя в диапазон атласа были
    // добавлены. Диапазон тут ни при чём: глифа нет в самом файле шрифта.
    // Поэтому вторым источником подмешивается системный, у которого он есть.
    // Тот же приём, что отложен для CJK в комментарии ниже.
    let arrows = ARROW_FONTS.iter().find_map(|p| std::fs::read(p).ok());
    ARROWS_BAKED.store(arrows.is_some(), std::sync::atomic::Ordering::Relaxed);

    let mut slot = |size_pixels: f32| {
        let sources = match bytes.as_deref() {
            Some(data) => {
                let mut v = vec![FontSource::TtfData {
                    data,
                    size_pixels: size_pixels.max(1.0),
                    config: Some(FontConfig {
                        // Латиница и кириллица плюс всё, что есть в файле
                        // активного перевода: без этого чужая локаль на
                        // польском или турецком вышла бы знаками вопроса.
                        // ponytail: иероглифы потребуют ещё и своего
                        // `font_path` - у pala.ttf их нет.
                        glyph_ranges: FontGlyphRanges::from_slice(crate::i18n::glyph_ranges()),
                        ..FontConfig::default()
                    }),
                }];
                if let Some(data) = arrows.as_deref() {
                    v.push(FontSource::TtfData {
                        data,
                        size_pixels: size_pixels.max(1.0),
                        config: Some(FontConfig {
                            // Второй источник в ОДНОМ `add_font` дописывает
                            // глифы в тот же слот: `add_font` сам ставит
                            // MergeMode всем, кроме первого. Отдельным вызовом
                            // поехала бы нумерация, на которой стоит `font_at`.
                            glyph_ranges: FontGlyphRanges::from_slice(&[0x2191, 0x2193, 0]),
                            ..FontConfig::default()
                        }),
                    });
                }
                v
            }
            None => vec![FontSource::DefaultFontData { config: None }],
        };
        ctx.fonts().add_font(&sources);
    };

    // Печём по самому крупному из кеглей: один слот обслуживает все, а
    // уменьшать запечённый глиф чище, чем растягивать.
    //
    // Подпись врага идёт с запасом x2: её кегль умножается на разрешение
    // (1080p -> 4K это ровно вдвое), а атлас про разрешение не знает - он
    // печётся в `initialize`, когда `display_size` ещё не выставлен.
    slot(
        cfg.value_size
            .max(cfg.counter_size)
            .max(cfg.label_size)
            .max(cfg.boss_name_size)
            .max(cfg.attempt_size)
            .max(cfg.toast_label_size)
            .max(cfg.toast_value_size)
            .max(cfg.enemy_tag_size * 2.0),
    );
    slot(SETTINGS_FONT_PX);
}

/// Шрифт окна настроек. Возвращает сырой указатель, потому что `FontId`
/// хранить нельзя (см. правило 2 в шапке модуля).
pub fn settings_font() -> Font {
    font_at(1)
}

// ---------------------------------------------------------------------------
// Панель
// ---------------------------------------------------------------------------

/// Диагностическое окно: стоковые виджеты ImGui, никакой нашей отрисовки и
/// никаких проверок видимости. Отвечает на вопрос "мод вообще жив?" - если
/// видно это окно, а обычной панели нет, значит дело в данных или в ручной
/// отрисовке, а не в загрузке DLL.

pub fn draw_debug(ui: &Ui, s: &Snapshot, frames: u64, suppressed: bool, probe: Vec<String>) {
    // Своим шрифтом фиксированного кегля, как окно настроек. По умолчанию
    // окно рисовалось шрифтом панели (слот 0), а он печётся по самому крупному
    // из кеглей HUD: при `ui_scale 1.7` диагностика выходила во весь экран
    // (жалоба 2026-08-20).
    let font = settings_font();
    let pushed = !font.is_null();
    if pushed {
        unsafe { imgui_sys::igPushFont(font as *mut _) };
    }
    /// Заголовок секции. Тот же золотой, что у панели.
    const HEAD: [f32; 4] = [0.87, 0.72, 0.44, 1.0];
    /// Строка, у которой нет своего смысла - продолжение предыдущей.
    const DIM: [f32; 4] = [0.62, 0.62, 0.58, 1.0];

    ui.window("game_information_counter debug")
        .position([24.0, 24.0], Condition::FirstUseEver)
        .always_auto_resize(true)
        .build(|| {
            // Живо ли вообще чтение памяти. Всё остальное имеет смысл только
            // при `valid`, поэтому строка ровно одна и стоит первой.
            let head = format!(
                "valid {} | кадр {frames} | панель {}",
                s.valid,
                if suppressed { "скрыта" } else { "видна" }
            );
            ui.text_colored(if s.valid { HEAD } else { [0.9, 0.4, 0.35, 1.0] }, &head);
            // Диагностику присылают скриншотом, а числа с него перенабирают
            // руками. Кнопка кладёт то же самое текстом.
            ui.same_line();
            if ui.small_button("копировать") {
                ui.set_clipboard_text(format!("{head}
{}", probe.join("
")));
            }

            // Дальше - то, что сейчас исследуется. Строки приходят готовыми из
            // `msg::probe`: "# " - заголовок секции, два пробела - продолжение.
            for line in &probe {
                match line.strip_prefix("# ") {
                    Some(head) => {
                        ui.separator();
                        ui.text_colored(HEAD, head);
                    }
                    None => match line.strip_prefix("  ") {
                        Some(tail) => {
                            ui.indent_by(12.0);
                            ui.text_colored(DIM, tail);
                            ui.unindent_by(12.0);
                        }
                        None => ui.text(line),
                    },
                }
            }
        });
    if pushed {
        unsafe { imgui_sys::igPopFont() };
    }
}

/// Одна метрика панели: подпись и значение.
#[derive(Debug)]
struct Item {
    label: String,
    /// Короткая форма для ленты, где место дороже полноты подписи.
    short: String,
    value: String,
}

/// Всё, что панель показывает, собранное один раз и независимо от компоновки.
struct Content {
    items: Vec<Item>,
    /// Ближайший живой босс: подпись - его имя, значение - метры. Рисуется НАД
    /// счётчиком убитых, а не среди метрик: это не «как дела», а «куда идти».
    nearest: Option<Item>,
    /// Счётчик боссов и доля пройденного.
    progress: Option<(String, f32)>,
    boss: Option<String>,
    /// Строки боя - такие же метрики, как всё остальное, и рисуются тем же
    /// кодом. Раньше это была одна свободная строка «Попытка N · время», и на
    /// панели она читалась инородно рядом со «СМЕРТИ»/«УРОВЕНЬ» (жалоба
    /// 2026-08-20: «Attempt, секундомер выглядят по-разному»).
    fight: Vec<Item>,
}

/// Стрелки высоты. Запечены в атлас отдельным диапазоном (см. `load_fonts`):
/// в `FontGlyphRanges::cyrillic()` их нет, и на экране вышли бы «?».
/// Откуда брать стрелки, если их нет в основном шрифте. Проверено живьём: у
/// всех трёх глифы есть, а лежат они в стандартной поставке Windows.
const ARROW_FONTS: [&str; 3] = [
    r"C:\Windows\Fonts\seguisym.ttf",
    r"C:\Windows\Fonts\segoeui.ttf",
    r"C:\Windows\Fonts\arial.ttf",
];

/// Удалось ли подмешать шрифт со стрелками. Не удалось - показываем знак
/// вместо стрелки: «?» на экране хуже плюса.
static ARROWS_BAKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const UP: char = '\u{2191}';
const DOWN: char = '\u{2193}';

/// Разница высот припиской к расстоянию: метры и стрелка вверх или вниз ЗА
/// ними (запрос 2026-09-03). Пусто - вровень.
///
/// Порог в пару метров съедает шум: ступенька под ногами не должна рисовать
/// стрелку. Сами стрелки запечены в атлас отдельно - см. `load_fonts`.
pub(crate) fn height_mark(dy: f32) -> String {
    if dy.abs() < 2.0 {
        return String::new();
    }
    let up = dy > 0.0;
    let arrow = match ARROWS_BAKED.load(std::sync::atomic::Ordering::Relaxed) {
        true => if up { UP } else { DOWN },
        false => if up { '+' } else { '-' },
    };
    format!(" {arrow}{:.0}", dy.abs())
}

/// Переносит длинное имя босса по словам: «плашка выглядит слишком длинной»
/// (жалоба 2026-09-07). Панель тянется по самой длинной строке, поэтому
/// перенос её и сужает.
///
/// По символам, а не по пикселям: `content` шрифта не знает вовсе, а кегль
/// имени настраивается отдельно от остальных - точная мерка потребовала бы
/// таскать сюда атлас. Слово длиннее лимита не режем: «Первородный» пополам
/// читается хуже, чем длинная строка.
///
/// `max = 0` - не переносить. Уже готовые переносы (двойной босс) остаются.
pub(crate) fn wrap_name(name: &str, max: usize) -> String {
    if max == 0 {
        return name.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    for line in name.lines() {
        let mut cur = String::new();
        for word in line.split_whitespace() {
            let room = cur.is_empty() || cur.chars().count() + 1 + word.chars().count() <= max;
            match room {
                true if cur.is_empty() => cur.push_str(word),
                true => {
                    cur.push(' ');
                    cur.push_str(word);
                }
                false => {
                    out.push(std::mem::take(&mut cur));
                    cur.push_str(word);
                }
            }
        }
        out.push(cur);
    }
    out.join("
")
}

/// То же ограничение для подписи метрики: она однострочная, поэтому режем, а
/// не переносим. Ноль - не трогать.
pub(crate) fn clip_name(name: &str, max: usize) -> String {
    match max {
        0 => name.to_string(),
        n => clip(name, n),
    }
}

fn content(s: &Snapshot, c: &Config) -> Content {
    let mut items = Vec::new();
    let mut push = |label: &str, short: &str, value: String| {
        items.push(Item { label: label.into(), short: short.into(), value })
    };

    if c.show_deaths {
        push(t("DEATHS"), t("deaths"), s.deaths.to_string());
    }
    if c.show_deaths_on_boss {
        push(t("DEATHS ON BOSSES"), t("boss deaths"), s.deaths_on_boss.to_string());
    }
    if c.show_viewer_kills {
        push(t("VIEWER KILLS"), t("by viewers"), s.viewer_deaths.to_string());
    }
    if c.show_level {
        push(t("LEVEL"), t("lvl"), s.level.to_string());
    }
    if c.show_runes {
        push(t("RUNES"), t("runes"), group_digits(s.runes));
    }
    if c.show_runes_total {
        push(t("RUNES TOTAL"), t("total"), group_digits(s.runes_total));
    }
    if c.show_playtime {
        push(t("PLAYTIME"), t("time"), fmt_hms(s.play_time_ms));
    }
    // NG показываем только со второго прохождения: "NG+0" - строка ни о чём.
    if c.show_ng && s.ng_lvl > 0 {
        push("NG+", "NG+", s.ng_lvl.to_string());
    }
    if c.show_deathless {
        push(t("DEATHLESS"), t("deathless"), fmt_mmss(s.deathless_secs));
    }
    if c.show_map_explored {
        let (visited, regions_total) = s.map_explored;
        let pct = if regions_total > 0 { visited as f32 / regions_total as f32 * 100.0 } else { 0.0 };
        push(t("MAP"), t("map"), format!("{pct:.0}%"));
    }

    let (killed, total) = match c.boss_count {
        BossCount::Named => s.bosses_named,
        BossCount::All => s.bosses_all,
    };
    let hours = s.play_time_ms as f64 / 3_600_000.0;
    // Комбинация боссов и времени игры: не то же самое, что просто счётчик
    // боссов или просто время каждый по отдельности - темп прохождения.
    if c.show_boss_kill_rate && hours > 0.0 {
        push(t("BOSSES / HOUR"), t("b/h"), format!("{:.1}", killed as f64 / hours));
    }
    if c.show_death_rate && hours > 0.0 {
        push(t("DEATHS / HOUR"), t("d/h"), format!("{:.1}", s.deaths as f64 / hours));
    }
    // Комбинация истории попыток (файл статистики) и числа убитых боссов:
    // сколько в среднем попыток стоит один босс за всю историю персонажа.
    if c.show_avg_attempts && killed > 0 {
        push(
            t("ATTEMPTS / BOSS"),
            t("att./boss"),
            format!("{:.1}", s.total_attempts as f32 / killed as f32),
        );
    }

    let progress = c.show_bosses.then(|| {
        let ratio = if total > 0 { killed as f32 / total as f32 } else { 0.0 };
        (format!("{killed}/{total}"), ratio)
    });

    let in_fight = s.boss_name.is_some();

    // Смерти на этом боссе и часы - обычные метрики, просто живут в блоке боя: тот же
    // `Item`, та же отрисовка строкой, тот же порядок «подпись слева,
    // значение справа». Своим кеглем (`attempt_size`) - он настраивается
    // отдельно, и это единственное, чем они отличаются.
    let mut fight = Vec::new();
    if in_fight {
        if c.show_attempts {
            // Номер попытки минус первый заход: смертей на этом боссе, с нуля.
            fight.push(Item {
                label: t("DEATHS##fight").into(),
                short: t("deaths##fight").into(),
                value: s.attempts.saturating_sub(1).to_string(),
            });
        }
        if c.show_fight_timer {
            fight.push(Item {
                label: t("FIGHT TIME").into(),
                short: t("fight").into(),
                value: fmt_mmss(s.attempt_secs),
            });
        }
    }
    Content {
        items,
        // Подпись - имя босса, значение - метры и высота. Высота отдельным
        // числом: босс прямо под ногами и босс в километре по прямой - разные
        // вещи, а одна цифра их смешивала.
        nearest: c.show_nearest_boss.then_some(s.nearest_boss.as_ref()).flatten().map(
            |(name, d, dy)| Item {
                // Переносим тем же лимитом, что и имя в бою: длинное имя
                // следующего босса тянуло плиту ровно так же. `short` идёт в
                // ленту B - она однострочная по определению, там режем.
                label: wrap_name(name, c.boss_name_wrap as usize),
                short: clip_name(name, c.boss_name_wrap as usize),
                value: format!("{d:.0}{}{}", t("m"), height_mark(*dy)),
            },
        ),
        progress,
        // Двойной босс приезжает двумя именами через перевод строки. Показываем
        // оба только по галочке: две строки заметно растят панель, а нужны не
        // всем (прямой запрос 2026-08-21).
        boss: if in_fight && c.show_boss_name {
            s.boss_name.as_ref().map(|n| match c.show_all_boss_names {
                true => wrap_name(n, c.boss_name_wrap as usize),
                false => wrap_name(n.lines().next().unwrap_or_default(), c.boss_name_wrap as usize),
            })
        } else {
            None
        },
        fight,
    }
}

impl Content {
    fn is_empty(&self) -> bool {
        self.items.is_empty() && self.progress.is_none() && !self.has_fight()
    }
    fn has_fight(&self) -> bool {
        self.boss.is_some() || !self.fight.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Компоновки. Отличаются только расстановкой; данные и примитивы общие.
// ---------------------------------------------------------------------------

/// Экспоненциальный ход к цели. Один шаг «на кадр» дал бы на 30 и на 144 fps
/// разную скорость - тест в `settings.rs` это и держит.
pub fn approach(now: f32, target: f32, dt: f32, secs: f32) -> f32 {
    now + (target - now) * (1.0 - (-dt / secs.max(0.001)).exp())
}

/// Сколько идёт смена размера панели. Блок боя появляется и исчезает целиком,
/// и без этого панель дёргалась в размерах скачком (жалоба 2026-08-21).
const PANEL_RESIZE_SECS: f32 = 0.16;

/// Текущий (анимированный) размер панели и «естественная» высота содержимого,
/// снятая в конце прошлого кадра.
///
/// Immediate mode: до отрисовки высота неизвестна, а размер окна задать надо
/// заранее - тот же приём и та же причина, что у `CARD_H` в окне настроек.
/// Ноль - панель на экране впервые: тогда размер берётся целевым сразу, иначе
/// она вырастала бы из точки при каждом появлении.
static PANEL_ANIM: std::sync::Mutex<[f32; 3]> = std::sync::Mutex::new([0.0, 0.0, 0.0]);

/// Положение панели по оси и не упёрлась ли она в дальний край: `(координата,
/// прижата)`.
///
/// **Прижатая остаётся прижатой, даже когда сузилась.** Панель у правого края
/// упирается в него широкой (появился босс), а сузившись возвращалась на
/// `panel_x` - то есть отходила от края на ширину исчезнувшей строки, и
/// выглядело это как «позиция сбилась сама» (скриншоты 2026-09-07). Держать
/// край честнее: пользователь ставил панель, глядя на прижатую.
///
/// Прижатие снимается тем, что положение подвинули руками - его помнит
/// `PANEL_PIN`, а не эта функция.
fn place(pos: f32, win: f32, screen: f32, pinned: bool) -> (f32, bool) {
    let edge = (screen - win).max(0.0);
    let over = pos > edge;
    match pinned || over {
        true => (edge, true),
        false => (pos.max(0.0), false),
    }
}

/// Где панель прижата к дальнему краю и при каком положении это решено.
static PANEL_PIN: std::sync::Mutex<([f32; 2], [bool; 2])> =
    std::sync::Mutex::new(([f32::NAN; 2], [false; 2]));

pub fn draw(ui: &Ui, s: &Snapshot, c: &Config) {
    let ct = content(s, c);
    if ct.is_empty() {
        return;
    }
    let font = font_at(0);
    let width = match c.layout {
        Layout::A => width_a(font, &ct, c),
        Layout::B => width_b(font, &ct, c),
        Layout::D => width_d(font, &ct, c),
    };

    let pad = [c.s(PANEL_PAD_X), c.s(PANEL_PAD_Y)];

    // Размер тянется к целевому, а содержимое рисуется по анимированной
    // ширине: значения разъезжаются плавно, а появившийся блок боя выезжает
    // из-под нижнего края - его обрезает clip rect самого окна.
    let dt = ui.io().delta_time.clamp(0.0, 0.1);
    let mut anim = PANEL_ANIM.lock().unwrap_or_else(|e| e.into_inner());
    let target_w = width;
    let target_h = anim[2];
    if target_h <= 0.0 {
        // Самый первый кадр: высоту содержимого ещё никто не мерил. Рисуем
        // «в ноль» - панель всё равно в этот момент прозрачная (`hud_fade`).
        anim[0] = target_w;
        anim[1] = 1.0;
    } else if anim[1] <= 1.0 {
        // Высота стала известна - берём её целиком, а не выращиваем с нуля:
        // появление панели ведёт затухание, а не размер.
        anim[0] = target_w;
        anim[1] = target_h;
    } else {
        anim[0] = approach(anim[0], target_w, dt, PANEL_RESIZE_SECS);
        anim[1] = approach(anim[1], target_h, dt, PANEL_RESIZE_SECS);
    }
    let (draw_w, win_h) = (anim[0], anim[1]);
    drop(anim);

    let style = ui.push_style_var(StyleVar::ItemSpacing([0.0, 0.0]));
    let padding = ui.push_style_var(StyleVar::WindowPadding(pad));
    // Плиту рисует `draw_panel_chrome`, поэтому фон и рамку ImGui выключаем.
    let bg = ui.push_style_color(hudhook::imgui::StyleColor::WindowBg, [0.0, 0.0, 0.0, 0.0]);
    let border = ui.push_style_var(StyleVar::WindowBorderSize(0.0));

    // Панель не должна вылезать за экран: положение задают ползунком, а
    // размер меняется сам (появился блок боя - стало выше и шире).
    let win = [draw_w + pad[0] * 2.0, win_h];
    let screen = ui.io().display_size;
    let at = {
        let mut pin = PANEL_PIN.lock().unwrap_or_else(|e| e.into_inner());
        // Положение подвинули руками - прижатие забываем.
        if pin.0 != [c.panel_x, c.panel_y] {
            *pin = ([c.panel_x, c.panel_y], [false; 2]);
        }
        let (x, px) = place(c.panel_x, win[0], screen[0], pin.1[0]);
        let (y, py) = place(c.panel_y, win[1], screen[1], pin.1[1]);
        pin.1 = [px, py];
        [x, y]
    };

    ui.window("##game_information_counter")
        .position(at, Condition::Always)
        .size(win, Condition::Always)
        .no_decoration()
        .no_inputs()
        .movable(false)
        .focus_on_appearing(false)
        .bring_to_front_on_focus(false)
        .build(|| {
            let dl = window_draw_list();
            draw_panel_chrome(dl, ui.window_pos(), ui.window_size(), c);
            match c.layout {
                Layout::A => layout_a(ui, dl, font, &ct, c, draw_w),
                Layout::B => layout_b(ui, dl, font, &ct, c, draw_w),
                Layout::D => layout_d(ui, dl, font, &ct, c, draw_w),
            }
            // Сколько содержимое заняло на самом деле - цель для следующего
            // кадра. `cursor_pos` идёт от верха клиентской области, поэтому
            // нижний отступ добавляем сами.
            if let Ok(mut anim) = PANEL_ANIM.lock() {
                anim[2] = ui.cursor_pos()[1] + pad[1];
            }
        });

    border.end();
    bg.end();
    padding.end();
    style.end();
}

/// Обрезка по СИМВОЛАМ, не байтам: ник зрителя бывает кириллицей и эмодзи, и
/// срез по байтам развалил бы UTF-8.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    // Три точки, а не U+2026: он вне FontGlyphRanges::cyrillic(), и ImGui
    // нарисовал бы вместо него знак вопроса - ровно те "?", что уже ловили в меню.
    s.chars().take(max).collect::<String>() + "..."
}

/// Сколько карточка гаснет в конце жизни.
const TOAST_FADE_SECS: f32 = 0.4;
/// Появление карточки: проявление и короткий подъезд справа.
const TOAST_IN_SECS: f32 = 0.22;

/// Карточки «зритель купил ...» в правом верхнем углу.
///
/// **Одна строка**: ник, название, справа отсчёт до появления врага либо цена.
/// Пять рядов с линейкой и названием по центру читались громоздко и занимали
/// пол-угла экрана (жалоба 2026-08-23). `toast_width` стал ПОТОЛКОМ ширины:
/// плита тянется по содержимому и не длиннее его.
///
/// Прозрачность у каждой своя, не общая с панелью: панель гаснет в инвентаре и
/// катсцене, а за покупку зритель заплатил - её надо показать в любом случае.
/// Поэтому зовётся ПОСЛЕ `draw`.
///
/// Общая прозрачность в конце **возвращается на место**. Она глобальная, а
/// следующий кадр начинается не с `set_alpha`: до него успевает нарисоваться
/// окно настроек, а в нём ручка положения панели идёт через тот же `col32` -
/// с догоревшей карточкой она пропадала бы на кадр при каждой покупке.
/// `ui_scale` карточку НЕ трогает: у неё свои три размера (кегли и ширина), и
/// общий масштаб панели, домножая их поверх, менял цифры на её же ползунках
/// (жалоба 2026-08-21). Крупнее нужна - двигают её собственные ползунки.
pub fn draw_purchase_toasts(ui: &Ui, events: &[crate::twitch::PurchaseEvent], c: &Config, life_secs: f32) {
    if events.is_empty() {
        return;
    }
    let restore = f32::from_bits(HUD_ALPHA_BITS.load(Ordering::Relaxed));
    let font = font_at(0);
    // Отступы и зазоры считаются от кегля самой карточки, а не константами
    // панели: иначе крупный текст упирался в края, а мелкий тонул в пустоте
    // (запрос 2026-08-23 - «адаптивные размеры под содержимое»).
    let em = c.toast_value_size.max(c.toast_label_size);
    let pad = [em * 0.75, em * 0.45];
    let gap = em * 0.4;
    let screen = ui.io().display_size;

    // Считаем вниз от заданной точки: свежая карточка сверху, старые уезжают
    // ниже и гаснут. Точку двигает ручка в настройках, как у панели.
    let mut y = c.toast_y;
    for (slot, e) in events.iter().rev().enumerate() {
        // Своё время жизни у покупки важнее общего: карточка временного
        // эффекта висит, пока эффект идёт.
        let life = e.life_secs(life_secs);
        let age = e.at.elapsed().as_secs_f32();
        if age >= life {
            continue;
        }
        let fade = ((life - age) / TOAST_FADE_SECS).clamp(0.0, 1.0);
        // Появление: карточка не возникает щелчком, а проявляется и
        // подъезжает справа - тем же жестом, что и всё остальное в моде.
        let appear = (age / TOAST_IN_SECS).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - appear).powi(3);
        set_alpha(fade * eased);

        let who = clip(&e.viewer, 18);
        // Справа одно число: пока враг не появился - отсчёт до него, дальше
        // цена. Два поля рядом пробовали 2026-08-25 и вернули обратно - в одну
        // строку они спорят друг с другом. Ноль - карточка о состоянии мода,
        // а не о покупке: справа пусто.
        let right_text = e
            .countdown_to
            .map(|at| at.saturating_duration_since(std::time::Instant::now()).as_secs_f32())
            .filter(|left| *left > 0.05)
            .map(|left| format!("{:.0} {}", left.ceil(), crate::i18n::t("s")))
            .unwrap_or_else(|| if e.cost == 0 { String::new() } else { format!("{}", e.cost) });

        // Одна строка, ширина по содержимому: карточка в пять рядов читалась
        // громоздко (жалоба 2026-08-23). `toast_width` теперь ПОТОЛОК ширины,
        // а не сама ширина - длинное название режется, короткое не тянет за
        // собой пустую плиту.
        let who_w = tracked_width(font, c.toast_label_size, c.tracking, &who);
        let right_w = measure_text(font, c.toast_label_size, &right_text)[0];
        let gap_x = em * 0.55;
        let room = c.toast_width - pad[0] * 2.0 - who_w - right_w - gap_x * 2.0;
        let what = clip_w(font, c.toast_value_size, &e.label, room.max(24.0));
        let what_w = measure_text(font, c.toast_value_size, &what)[0];

        let row_h = em * 1.3;
        let card_w =
            (pad[0] * 2.0 + who_w + what_w + right_w + gap_x * 2.0).min(c.toast_width);
        let size = [card_w, row_h + pad[1] * 2.0];
        // Прижимаем к экрану: дефолт задан для 1080p, и на меньшем
        // разрешении карточка иначе уехала бы за правый край целиком.
        let x = c.toast_x.min(screen[0] - size[0]).max(0.0) + (1.0 - eased) * 16.0;

        let style = ui.push_style_var(StyleVar::ItemSpacing([0.0, 0.0]));
        let padding = ui.push_style_var(StyleVar::WindowPadding(pad));
        let bg = ui.push_style_color(hudhook::imgui::StyleColor::WindowBg, [0.0, 0.0, 0.0, 0.0]);
        let border = ui.push_style_var(StyleVar::WindowBorderSize(0.0));
        // Id по месту в стопке, а не по времени: меняющийся каждый кадр id
        // заставлял бы ImGui заводить новое окно на каждый кадр.
        ui.window(format!("##toast{slot}"))
            .position([x, y], Condition::Always)
            .size(size, Condition::Always)
            .no_decoration()
            .no_inputs()
            .movable(false)
            .focus_on_appearing(false)
            .bring_to_front_on_focus(false)
            .build(|| {
                let dl = window_draw_list();
                let pos = ui.window_pos();
                draw_chrome(dl, pos, ui.window_size(), c, Chrome::toast(c));
                // Текст от фактического положения окна, а не от заданного:
                // ImGui вправе подвинуть окно, и тогда плита с текстом
                // разъехались бы.
                let tx = pos[0] + pad[0];
                let right = pos[0] + ui.window_size()[0] - pad[0];
                // Кегли в строке разные, поэтому общая базовая линия - иначе
                // ник и название стоят «горкой» (правило из шапки модуля).
                //
                // Строка ВЫШЕ самого крупного кегля (`row_h`), и без поправки
                // текст прижимался к её верху: сверху ноль, снизу треть кегля
                // (жалоба 2026-08-23 - «стоит не симметрично»). Поэтому блок
                // текста центрируется в строке.
                let top = pos[1] + pad[1] + (row_h - em) * 0.5;
                let base = row_baseline(font, top, &[c.toast_label_size, c.toast_value_size]);
                let put = |size_px: f32| top_for_baseline(font, size_px, base);

                text_tracked(dl, font, [tx, put(c.toast_label_size)], c.label_color, c.toast_label_size, c.tracking, &who);
                text_shadowed(dl, font, [tx + who_w + gap_x, put(c.toast_value_size)], c.value_color, c.toast_value_size, &what);
                text_shadowed(dl, font, [right - right_w, put(c.toast_label_size)], c.accent_color, c.toast_label_size, &right_text);
            });
        border.end();
        bg.end();
        padding.end();
        style.end();

        y += size[1] + gap;
    }
    set_alpha(restore);
}

/// Оверлей союзников: кто прислал, кого прислал, здоровье и остаток жизни.
///
/// **Своя отрисовка, а не список пеплов праха слева.** Запись в
/// `FrontEndViewValues::spirit_ash_display` пробовали живьём 2026-09-07 и на
/// экране не появилось ничего: буфер забирает scaleform раньше, чем мод
/// доходит до `Present`. Та же стена, что была с `CSCamera::pers_cam_1.fov`.
///
/// Своим окном на своей позиции - как карточки покупок, и по той же причине:
/// это отдельный источник, который двигают отдельно от панели.
pub fn draw_allies(ui: &Ui, allies: &[crate::spawn::AllyView], c: &Config) {
    if allies.is_empty() {
        return;
    }
    // Своя прозрачность, и общую надо вернуть - иначе она утекает на всё, что
    // рисуется дальше в этом кадре. Ровно так же делают подписи врагов и
    // карточки покупок.
    let restore = f32::from_bits(HUD_ALPHA_BITS.load(Ordering::Relaxed));
    set_alpha(1.0);
    let font = font_at(0);
    let em = c.value_size.max(c.label_size);
    let pad = [em * 0.75, em * 0.45];
    let row_gap = em * 0.45;
    // Заметно толще панельной полоски прогресса: у игры она плотная, и на
    // тонкой линии «хвост урона» не читался бы вовсе.
    let bar_h = (em * 0.45).max(5.0);
    let bar_gap = em * 0.22;
    let screen = ui.io().display_size;

    // Ширина по самой длинной строке, но не уже разумного: полоска здоровья в
    // тридцать пикселей не читается вовсе.
    let mut inner = 120.0f32;
    let rows: Vec<(String, String, (f32, f32))> = allies
        .iter()
        .map(|a| {
            let who = clip(&a.viewer, 14);
            let who = if who.is_empty() { a.name.clone() } else { format!("{who} - {}", a.name) };
            let left = format!("{:.0} {}", a.secs_left.max(0.0).ceil(), crate::i18n::t("s"));
            let w = tracked_width(font, c.label_size, c.tracking, &who)
                + measure_text(font, c.label_size, &left)[0]
                + em * 0.8;
            inner = inner.max(w);
            (who, left, (a.hp as f32 / a.max_hp.max(1) as f32, a.lag))
        })
        .collect();

    let line_h = em + bar_gap + bar_h;
    let size = [
        inner + pad[0] * 2.0,
        rows.len() as f32 * line_h + (rows.len().saturating_sub(1)) as f32 * row_gap + pad[1] * 2.0,
    ];
    // Прижимаем к экрану, как панель и карточки: дефолт задан для 1080p.
    let pos = [
        c.ally_x.min(screen[0] - size[0]).max(0.0),
        c.ally_y.min(screen[1] - size[1]).max(0.0),
    ];

    let style = ui.push_style_var(StyleVar::ItemSpacing([0.0, 0.0]));
    let padding = ui.push_style_var(StyleVar::WindowPadding(pad));
    let bg = ui.push_style_color(hudhook::imgui::StyleColor::WindowBg, [0.0, 0.0, 0.0, 0.0]);
    let border = ui.push_style_var(StyleVar::WindowBorderSize(0.0));
    ui.window("##allies")
        .position(pos, Condition::Always)
        .size(size, Condition::Always)
        .no_decoration()
        .no_inputs()
        .movable(false)
        .focus_on_appearing(false)
        .bring_to_front_on_focus(false)
        .build(|| {
            let dl = window_draw_list();
            let at = ui.window_pos();
            draw_panel_chrome(dl, at, ui.window_size(), c);
            // Текст от фактического положения окна: ImGui вправе его подвинуть.
            let x = at[0] + pad[0];
            let right = at[0] + ui.window_size()[0] - pad[0];
            let mut y = at[1] + pad[1];
            for (who, left, ratio) in &rows {
                let base = row_baseline(font, y, &[c.label_size]);
                text_tracked(
                    dl,
                    font,
                    [x, top_for_baseline(font, c.label_size, base)],
                    c.label_color,
                    c.label_size,
                    c.tracking,
                    who,
                );
                let lw = measure_text(font, c.label_size, left)[0];
                text_shadowed(
                    dl,
                    font,
                    [right - lw, top_for_baseline(font, c.label_size, base)],
                    c.accent_color,
                    c.label_size,
                    left,
                );
                draw_hp_bar(dl, x, y + em + bar_gap, right - x, bar_h, ratio.0, ratio.1, c);
                y += line_h + row_gap;
            }
        });
    border.end();
    bg.end();
    padding.end();
    style.end();
    set_alpha(restore);
}

/// Никнеймы зрителей над врагами.
///
/// Рисуется в своём прозрачном окне на весь экран - иначе позиция каждой
/// подписи зависела бы от авторазмерного окна панели. Общая прозрачность
/// панели сюда не подмешивается: подписи привязаны к врагам, а не к HUD.
pub fn draw_enemy_tags(ui: &Ui, tags: &[(String, Option<(String, f32)>, [f32; 2])], c: &Config) {
    if tags.is_empty() {
        return;
    }
    let restore = f32::from_bits(HUD_ALPHA_BITS.load(Ordering::Relaxed));
    set_alpha(1.0);

    let font = font_at(0);
    let screen = ui.io().display_size;
    let style = ui.push_style_var(StyleVar::WindowPadding([0.0, 0.0]));
    let bg = ui.push_style_color(hudhook::imgui::StyleColor::WindowBg, [0.0, 0.0, 0.0, 0.0]);
    let border = ui.push_style_var(StyleVar::WindowBorderSize(0.0));

    // Подпись центрируется по позиции тега и поднимается над полоской HP:
    // игра даёт левый край полоски, а не середину надписи.
    ui.window("##enemy_tags")
        .position([0.0, 0.0], Condition::Always)
        .size(screen, Condition::Always)
        .no_decoration()
        .no_inputs()
        .movable(false)
        .focus_on_appearing(false)
        .bring_to_front_on_focus(false)
        .build(|| {
            let dl = window_draw_list();
            // Кегль задан для 1080p и растёт вместе с экраном: полоска HP у
            // игры масштабируется с разрешением, и подпись обязана вести себя
            // так же, иначе на 2K она выглядит мелкой приписькой к полоске.
            // Тем же масштабом, что и координаты, - иначе на не-16:9 подпись и
            // её сдвиг разъедутся.
            let (k, _) = crate::enemies::ui_scale(screen);
            for (name, said, pos) in tags {
                let size = (c.enemy_tag_size * k).max(6.0);
                // От ЛЕВОГО края: игра даёт левый край полоски HP, поэтому
                // подписи разной длины начинаются в одной точке, а не разъезжаются
                // в стороны, как при центрировании. Подогнать под свою полоску -
                // слайдером «сдвиг по горизонтали».
                //
                // По вертикали центрируем по точке: тогда смена кегля меняет
                // размер надписи, но не её место. Раньше высота вычиталась, и
                // ник уезжал вверх при каждом увеличении шрифта.
                // По бокам подпись держится в тех же полях, что и полоска у
                // игры, - то же самое, что `BAR_TOP_LIMIT` делает сверху.
                //
                // Считается по САМОЙ ПОДПИСИ, а не по якорю: она тянется
                // вправо на свою ширину, и у правого края длинный ник вылезал
                // бы за экран там, где короткий помещается. Поэтому справа из
                // поля вычитается ширина текста (запрос 2026-08-24 - «учитывать
                // разную длину никнеймов»).
                //
                // В середине экрана кламп не срабатывает вовсе, поэтому
                // «Сдвиг вбок» остаётся ровно тем, что выставил пользователь.
                let name_w = measure_text(font, size, name)[0];
                let m = crate::enemies::BAR_SIDE_MARGIN * k;
                let at = [
                    pos[0].clamp(m, (screen[0] - m - name_w).max(m)),
                    pos[1] - size * 0.5,
                ];
                text_shadowed(dl, font, at, c.enemy_tag_color, size, name);
                // «Последнее слово»: реплика этого зрителя из чата, строкой
                // ниже и мельче. Своего ползунка кегля у неё нет намеренно -
                // она обязана читаться как приписка к нику, а не спорить с ним.
                if let Some((said, fade)) = said {
                    let small = (size * 0.8).max(6.0);
                    let quote = clip_w(font, small, said, screen[0] * 0.25);
                    // Сдвиг в тех же виртуальных единицах, что и у самого ника:
                    // числом в пикселях экрана реплика уезжала бы по-разному на
                    // 1080p и на 2K.
                    let sx = at[0] + c.enemy_say_offset_x * k;
                    let sy = at[1] + size + c.enemy_say_offset_y * k;
                    // Гаснет своей прозрачностью, а не общей `set_alpha`: она
                    // одна на все подписи в кадре, а реплики у каждого врага
                    // своего возраста.
                    text_shadowed(dl, font, [sx, sy], with_alpha(c.label_color, c.label_color[3] * fade), small, &quote);
                }
            }
        });

    border.end();
    bg.end();
    style.end();
    set_alpha(restore);
}

/// Ширину контента считаем сами, а не берём из `content_region_avail()`: окно
/// авторазмерное, его ширина выводится из содержимого, и обратный вывод
/// зацикливался бы от кадра к кадру.
fn header_width(font: Font, ct: &Content, c: &Config) -> f32 {
    let near = ct.nearest.as_ref().map_or(0.0, |i| row_width(font, i, c, c.value_size));
    near.max(progress_width(font, ct, c))
}

fn progress_width(font: Font, ct: &Content, c: &Config) -> f32 {
    match &ct.progress {
        Some((text, _)) => {
            let (killed, rest) = split_boss_counter(text);
            tracked_width(font, c.label_size, c.s(c.tracking), progress_label())
                + c.s(MIN_GAP)
                + measure_text(font, c.counter_size, killed)[0]
                + measure_text(font, c.label_size, rest)[0]
        }
        None => 0.0,
    }
}

/// Одна строка списка: подпись слева, значение справа, обе на ОБЩЕЙ базовой
/// линии - выравнивание по верху разводит кегли «горкой».
///
/// Общая для метрик и для строк боя. Раньше «Попытка N · время» рисовалась
/// своим кодом одной свободной строкой и рядом со «СМЕРТИ»/«УРОВЕНЬ» читалась
/// инородно (жалоба 2026-08-20). Теперь разница ровно одна - кегль значения.
#[allow(clippy::too_many_arguments)]
/// Зазор между строками длинной подписи. Меньше межстрочного у метрик: это
/// одна подпись, а не две.
fn label_line_h(c: &Config) -> f32 {
    c.label_size + c.s(2.0)
}

fn draw_row(ui: &Ui, dl: DrawList, font: Font, item: &Item, c: &Config, w: f32, value_size: f32, trailing: f32) {
    let [x0, y] = ui.cursor_screen_pos();
    let base = row_baseline(font, y, &[value_size, c.label_size]);
    // Длинное имя босса переносится, а не обрезается: «то, что не вместилось,
    // просто скрывается» - прямая жалоба 2026-09-07. Значение остаётся на
    // первой строке: это метры, и они относятся к строке целиком.
    let mut lines = item.label.lines();
    let first = lines.next().unwrap_or_default();
    text_tracked(
        dl,
        font,
        [x0, top_for_baseline(font, c.label_size, base)],
        c.label_color,
        c.label_size,
        c.s(c.tracking),
        first,
    );
    let vw = measure_text(font, value_size, &item.value)[0];
    text_shadowed(
        dl,
        font,
        [x0 + w - vw, top_for_baseline(font, value_size, base)],
        c.value_color,
        value_size,
        &item.value,
    );
    let head = value_size.max(c.label_size);
    let mut extra = 0.0;
    for line in lines {
        extra += label_line_h(c);
        text_tracked(
            dl,
            font,
            [x0, y + head + extra - c.label_size],
            c.label_color,
            c.label_size,
            c.s(c.tracking),
            line,
        );
    }
    ui.dummy([w, head + extra + trailing]);
}

/// Ширина такой строки: подпись, зазор, значение.
///
/// У переносимой подписи ширину задаёт либо первая строка вместе со значением,
/// либо самая длинная из остальных - значение стоит только на первой.
fn row_width(font: Font, item: &Item, c: &Config, value_size: f32) -> f32 {
    let tracking = c.s(c.tracking);
    let mut lines = item.label.lines();
    let head = tracked_width(font, c.label_size, tracking, lines.next().unwrap_or_default())
        + c.s(MIN_GAP)
        + measure_text(font, value_size, &item.value)[0];
    lines.map(|l| tracked_width(font, c.label_size, tracking, l)).fold(head, f32::max)
}

/// Одна ячейка двухколоночной раскладки: значение крупно, подпись мелко под
/// ним. Тоже общая - строки боя в компоновке A обязаны быть такими же
/// ячейками, а не отдельной строкой снизу.
fn draw_cell(dl: DrawList, font: Font, item: &Item, c: &Config, x: f32, vbase: f32, value_size: f32) {
    text_shadowed(
        dl,
        font,
        [x, top_for_baseline(font, value_size, vbase)],
        c.value_color,
        value_size,
        &item.value,
    );
    let lbase = vbase + c.label_size + c.s(2.0);
    text_tracked(
        dl,
        font,
        [x, top_for_baseline(font, c.label_size, lbase)],
        c.label_color,
        c.label_size,
        c.s(c.tracking),
        &item.label,
    );
}

/// Ширина самой широкой ячейки в наборе.
fn cells_width(font: Font, items: &[Item], c: &Config, value_size: f32) -> f32 {
    items
        .iter()
        .map(|i| {
            measure_text(font, value_size, &i.value)[0]
                .max(tracked_width(font, c.label_size, c.s(c.tracking), &i.label))
        })
        .fold(0.0, f32::max)
}

/// Имена боссов для ленты B: она в одну строку, и столбик туда не влезает.
/// В A и D имена идут строками, каждое своей.
fn strip_name(name: &str) -> String {
    name.lines().collect::<Vec<_>>().join(" \u{b7} ").to_uppercase()
}

/// Ширина блока боя. `cells` - раскладка A (две колонки), иначе список D.
fn fight_width(font: Font, ct: &Content, c: &Config, cells: bool) -> f32 {
    // Двойной босс - две строки, ширина по самой длинной.
    let name = ct.boss.as_ref().map_or(0.0, |n| {
        n.lines()
            .map(|l| tracked_width(font, c.boss_name_size, c.s(c.tracking), &l.to_uppercase()))
            .fold(0.0, f32::max)
    });
    let body = if cells {
        if ct.fight.is_empty() {
            0.0
        } else {
            cells_width(font, &ct.fight, c, c.attempt_size) * 2.0 + c.s(MIN_GAP)
        }
    } else {
        ct.fight.iter().map(|i| row_width(font, i, c, c.attempt_size)).fold(0.0, f32::max)
    };
    name.max(body)
}

/// Общий хвост A и D: разделитель, имя босса заголовком, строки боя.
///
/// `cells` выбирает вид строк боя - ячейками (A) или списком (D). Своей
/// отрисовки у них больше нет: они рисуются ровно тем же кодом, что и
/// обычные метрики выше.
#[allow(clippy::too_many_arguments)]
fn draw_fight_block(
    ui: &Ui,
    dl: DrawList,
    font: Font,
    ct: &Content,
    c: &Config,
    w: f32,
    divider: bool,
    cells: bool,
) {
    let gap = c.s(c.line_gap);
    if divider {
        let [x, y] = ui.cursor_screen_pos();
        draw_divider(dl, x, y + gap, w, c.accent_color);
        ui.dummy([w, gap * 2.0 + 1.0]);
    }
    if let Some(name) = &ct.boss {
        // Строка на босса: у двойного второе имя идёт ниже, а не продолжает
        // первое.
        for (i, line) in name.lines().enumerate() {
            let [x, y] = ui.cursor_screen_pos();
            // Без отступа: имя рельса не рисует, и вровень с остальным текстом
            // левый край читается тем же, что и правый (см. шапку раздела про
            // асимметричные отступы).
            text_tracked(
                dl,
                font,
                [x, y],
                c.accent_color,
                c.boss_name_size,
                c.s(c.tracking),
                &line.to_uppercase(),
            );
            // Между именами зазор меньше: это один блок, а не две метрики.
            let trailing = if i + 1 == name.lines().count() { gap } else { gap * 0.4 };
            ui.dummy([w, c.boss_name_size + trailing]);
        }
    }
    let size = c.attempt_size;
    if cells {
        let cell_w = cells_width(font, &ct.fight, c, size);
        let cell_h = size + c.label_size + c.s(2.0);
        for pair in ct.fight.chunks(2) {
            let [x, y] = ui.cursor_screen_pos();
            let vbase = row_baseline(font, y, &[size]);
            for (i, item) in pair.iter().enumerate() {
                draw_cell(dl, font, item, c, x + i as f32 * (cell_w + c.s(MIN_GAP)), vbase, size);
            }
            ui.dummy([w, cell_h + gap]);
        }
    } else {
        let last = ct.fight.len().saturating_sub(1);
        for (i, item) in ct.fight.iter().enumerate() {
            draw_row(ui, dl, font, item, c, w, size, if i == last { 0.0 } else { gap });
        }
    }
}

/// Шапка A и D: ближайший босс, под ним линейка, затем подпись секции слева,
/// счётчик крупно справа и полоска прогресса под ними (её можно выключить
/// отдельно от счётчика - `show_boss_bar`).
///
/// Линейка едет вместе с ближайшим боссом: она отделяет «куда идти» от «как
/// дела», и без него отделять нечего. Собственного подчёркивания у заголовка
/// секции больше нет - две горизонтали подряд читались как рябь.
fn draw_progress_head(ui: &Ui, dl: DrawList, font: Font, ct: &Content, c: &Config, w: f32) {
    // Ближайший босс идёт НАД счётчиком: под полоской прогресса он читался как
    // ещё одна метрика, а это указание, куда идти.
    if let Some(near) = &ct.nearest {
        // Один и тот же зазор вокруг линейки и вокруг полоски: разные числа
        // давали видимый перекос сверху и снизу секции.
        let gap = section_gap(c);
        draw_row(ui, dl, font, near, c, w, c.value_size, gap);
        rule_line(ui.cursor_screen_pos(), w, with_alpha(c.label_color, 0.3));
        ui.dummy([w, gap]);
    }
    let Some((text, ratio)) = &ct.progress else {
        return;
    };
    draw_section_header(ui, dl, font, progress_label(), Some(split_boss_counter(text)), c, w);
    if !c.show_boss_bar {
        return;
    }
    let bar_h = c.s(5.0);
    let [x, y] = ui.cursor_screen_pos();
    draw_progress(dl, x, y, w, bar_h, *ratio, c);
    ui.dummy([w, bar_h + section_gap(c)]);
}

// --- A: значение крупно, подпись мелко под ним, две колонки -----------------

fn width_a(font: Font, ct: &Content, c: &Config) -> f32 {
    let grid = if ct.items.is_empty() {
        0.0
    } else {
        cells_width(font, &ct.items, c, c.value_size) * 2.0 + c.s(MIN_GAP)
    };
    grid.max(header_width(font, ct, c)).max(fight_width(font, ct, c, true))
}

fn layout_a(ui: &Ui, dl: DrawList, font: Font, ct: &Content, c: &Config, w: f32) {
    let gap = c.s(c.line_gap);
    let cell_w = cells_width(font, &ct.items, c, c.value_size);
    let cell_h = c.value_size + c.label_size + c.s(2.0);

    draw_progress_head(ui, dl, font, ct, c, w);

    // Две колонки: панель выходит вдвое короче списка при том же содержимом.
    for pair in ct.items.chunks(2) {
        let [x, y] = ui.cursor_screen_pos();
        let vbase = row_baseline(font, y, &[c.value_size]);
        for (i, item) in pair.iter().enumerate() {
            draw_cell(dl, font, item, c, x + i as f32 * (cell_w + c.s(MIN_GAP)), vbase, c.value_size);
        }
        ui.dummy([w, cell_h + gap]);
    }

    if ct.has_fight() {
        draw_fight_block(ui, dl, font, ct, c, w, !ct.items.is_empty() || ct.progress.is_some(), true);
    }
}

// --- B: горизонтальная лента ------------------------------------------------

/// Ширина ленты: всё в одну строку, поэтому складываем, а не берём максимум.
fn strip_width(font: Font, ct: &Content, c: &Config) -> f32 {
    let gap = c.s(MIN_GAP);
    let mut w = 0.0;
    if let Some((text, _)) = &ct.progress {
        // Полоски в ленте нет: рядом с самой цифрой она ничего не добавляет,
        // а место в одну строку дорогое (убрана по отзыву 2026-08-18).
        w += measure_text(font, c.counter_size, text)[0] + gap;
    }
    for item in &ct.items {
        w += tracked_width(font, c.label_size, c.s(c.tracking), &item.short)
            + c.s(5.0)
            + measure_text(font, c.value_size, &item.value)[0]
            + gap;
    }
    (w - gap).max(0.0)
}

fn fight_strip_width(font: Font, ct: &Content, c: &Config) -> f32 {
    let gap = c.s(MIN_GAP);
    // Лента - одна строка по определению, поэтому у двойного босса имена идут
    // в неё подряд, а не столбиком (см. `strip_name`).
    let name = ct.boss.as_ref().map_or(0.0, |n| {
        tracked_width(font, c.boss_name_size, c.s(c.tracking), &strip_name(n)) + gap
    });
    let items: f32 = ct
        .fight
        .iter()
        .map(|i| {
            tracked_width(font, c.label_size, c.s(c.tracking), &i.short)
                + c.s(5.0)
                + measure_text(font, c.attempt_size, &i.value)[0]
                + gap
        })
        .sum();
    name + (items - gap).max(0.0)
}

fn width_b(font: Font, ct: &Content, c: &Config) -> f32 {
    strip_width(font, ct, c)
        .max(fight_strip_width(font, ct, c))
}

fn layout_b(ui: &Ui, dl: DrawList, font: Font, ct: &Content, c: &Config, w: f32) {
    let gap = c.s(MIN_GAP);
    let [x0, y] = ui.cursor_screen_pos();
    let mut x = x0;
    // Одна базовая линия на всю ленту: без этого подписи и значения разных
    // кеглей стояли "горкой".
    let base = row_baseline(font, y, &[c.counter_size, c.value_size, c.label_size]);
    let row_h = c.counter_size.max(c.value_size);

    if let Some((text, _)) = &ct.progress {
        text_shadowed(
            dl,
            font,
            [x, top_for_baseline(font, c.counter_size, base)],
            c.accent_color,
            c.counter_size,
            text,
        );
        x += measure_text(font, c.counter_size, text)[0] + gap;
    }
    for item in &ct.items {
        x += text_tracked(
            dl,
            font,
            [x, top_for_baseline(font, c.label_size, base)],
            c.label_color,
            c.label_size,
            c.s(c.tracking),
            &item.short,
        );
        x += c.s(5.0);
        text_shadowed(
            dl,
            font,
            [x, top_for_baseline(font, c.value_size, base)],
            c.value_color,
            c.value_size,
            &item.value,
        );
        x += measure_text(font, c.value_size, &item.value)[0] + gap;
    }
    ui.dummy([w, row_h + c.s(2.0)]);

    if ct.has_fight() {
        let gap_v = c.s(c.line_gap);
        let [dx, dy] = ui.cursor_screen_pos();
        draw_divider(dl, dx, dy + gap_v, w, c.accent_color);
        ui.dummy([w, gap_v * 2.0 + 1.0]);

        let [fx0, fy] = ui.cursor_screen_pos();
        let mut fx = fx0;
        let fbase = row_baseline(font, fy, &[c.attempt_size, c.boss_name_size]);
        if let Some(name) = &ct.boss {
            fx += text_tracked(
                dl,
                font,
                [fx, top_for_baseline(font, c.boss_name_size, fbase)],
                c.accent_color,
                c.boss_name_size,
                c.s(c.tracking),
                &strip_name(name),
            );
            fx += gap;
        }
        // Те же пары «подпись - значение», что и у метрик в ленте выше:
        // отличается только кегль значения (`attempt_size`).
        for item in &ct.fight {
            fx += text_tracked(
                dl,
                font,
                [fx, top_for_baseline(font, c.label_size, fbase)],
                c.label_color,
                c.label_size,
                c.s(c.tracking),
                &item.short,
            );
            fx += c.s(5.0);
            text_shadowed(
                dl,
                font,
                [fx, top_for_baseline(font, c.attempt_size, fbase)],
                c.value_color,
                c.attempt_size,
                &item.value,
            );
            fx += measure_text(font, c.attempt_size, &item.value)[0] + gap;
        }
        ui.dummy([w, c.attempt_size.max(c.boss_name_size) + c.s(2.0)]);
    }
}

// --- D: список «подпись слева, значение справа» -----------------------------

fn width_d(font: Font, ct: &Content, c: &Config) -> f32 {
    let rows = ct.items.iter().map(|i| row_width(font, i, c, c.value_size)).fold(0.0, f32::max);
    rows.max(header_width(font, ct, c)).max(fight_width(font, ct, c, false))
}

fn layout_d(ui: &Ui, dl: DrawList, font: Font, ct: &Content, c: &Config, w: f32) {
    let gap = c.s(c.line_gap);

    draw_progress_head(ui, dl, font, ct, c, w);

    let last = ct.items.len().saturating_sub(1);
    for (i, item) in ct.items.iter().enumerate() {
        let trailing = if i == last && !ct.has_fight() { 0.0 } else { gap };
        draw_row(ui, dl, font, item, c, w, c.value_size, trailing);
    }

    if ct.has_fight() {
        draw_fight_block(ui, dl, font, ct, c, w, !ct.items.is_empty() || ct.progress.is_some(), false);
    }
}

// ---------------------------------------------------------------------------

pub fn fmt_hms(ms: u32) -> String {
    let total = ms / 1000;
    format!("{}:{:02}:{:02}", total / 3600, (total / 60) % 60, total % 60)
}

pub fn fmt_mmss(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// Руны по три разряда через ОБЫЧНЫЙ пробел. Узкая неразрывная шпация
/// смотрелась бы лучше, но её нет в Palatino, и вместо неё рисуется "?" -
/// живьём это выглядело как "42?084" (2026-08-17). Любой символ, которого
/// может не быть в пользовательском шрифте, здесь запрещён.
pub fn group_digits(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hms_pads_minutes_and_seconds() {
        assert_eq!(fmt_hms(0), "0:00:00");
        assert_eq!(fmt_hms(61_000), "0:01:01");
        assert_eq!(fmt_hms(3_600_000), "1:00:00");
        // Игра капается на 999:59:59.999, часы не переносим в дни.
        assert_eq!(fmt_hms(359_999_999), "99:59:59");
    }

    #[test]
    fn mmss_never_goes_negative() {
        assert_eq!(fmt_mmss(0.0), "0:00");
        assert_eq!(fmt_mmss(-5.0), "0:00");
        assert_eq!(fmt_mmss(65.9), "1:05");
    }

    #[test]
    fn splits_boss_counter_at_the_slash() {
        assert_eq!(split_boss_counter("17/206"), ("17", "/206"));
        assert_eq!(split_boss_counter("0/0"), ("0", "/0"));
        // Без '/' не паникует - делит в конец строки.
        assert_eq!(split_boss_counter("206"), ("206", ""));
    }

    #[test]
    fn digits_group_by_three() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1 000");
        assert_eq!(group_digits(1_234_567), "1 234 567");
        // Только ASCII: экзотического пробела может не быть в шрифте.
        assert!(group_digits(1_234_567).is_ascii());
    }

    /// Галочки в конфиге должны реально убирать метрики, а не просто прятать
    /// значение - иначе панель остаётся в полный рост с пустотами.
    #[test]
    fn toggles_remove_items() {
        let s = Snapshot::default();
        let mut c = Config::default();
        let full = content(&s, &c).items.len();
        c.show_deaths = false;
        c.show_level = false;
        assert_eq!(content(&s, &c).items.len(), full - 2);
    }

    /// NG+ на первом прохождении - строка ни о чём, её быть не должно.
    #[test]
    fn ng_item_only_past_the_first_playthrough() {
        let mut s = Snapshot::default();
        let mut c = Config::default();
        c.show_ng = true;
        assert!(!content(&s, &c).items.iter().any(|i| i.label == "NG+"));
        s.ng_lvl = 1;
        assert!(content(&s, &c).items.iter().any(|i| i.label == "NG+"));
    }

    /// Блок боя появляется только когда есть с кем драться: пустая секция с
    /// разделителем выглядит поломкой.
    #[test]
    fn each_pad_owns_its_drag() {
        // Слот перетаскивания один на окно, а ручек в разделе «Панель» две.
        assert_ne!(pad_key("##panel_xy"), pad_key("##toast_xy"));
        assert_eq!(pad_key("##panel_xy"), pad_key("##panel_xy"));
    }

    #[test]
    fn message_fades_out_and_then_disappears() {
        // Свежая - в полную силу, на исходе гаснет, после срока её нет вовсе.
        assert_eq!(crate::fade_out(0.0, 10.0), Some(1.0));
        let half = crate::fade_out(9.6, 10.0).expect("на исходе она ещё видна");
        assert!((half - 0.5).abs() < 0.01, "{half}");
        assert_eq!(crate::fade_out(10.0, 10.0), None);
        assert_eq!(crate::fade_out(99.0, 10.0), None);
    }

    #[test]
    fn second_boss_name_is_opt_in() {
        let mut s = Snapshot::default();
        s.boss_name = Some("Первый
Второй".into());
        let mut c = Config::default();
        assert_eq!(content(&s, &c).boss.as_deref(), Some("Первый"), "по умолчанию - только первый");
        c.show_all_boss_names = true;
        assert_eq!(content(&s, &c).boss.as_deref(), Some("Первый
Второй"));
    }

    #[test]
    fn preview_sample_fills_the_fight_block() {
        // Ползунки «Имя босса» и «Попытка и время» показывают образец боя -
        // если он перестанет давать блок боя, настраивать их снова будет вслепую.
        let mut s = Snapshot::default();
        crate::demo_fight(&mut s);
        let c = Config::default();
        let ct = content(&s, &c);
        assert!(ct.boss.is_some(), "в образце нет имени босса");
        assert_eq!(ct.fight.len(), 2, "в образце нет попытки и часов");
    }

    #[test]
    fn fight_block_needs_a_boss() {
        let mut s = Snapshot::default();
        let c = Config::default();
        assert!(!content(&s, &c).has_fight(), "без босса блока боя быть не должно");
        s.boss_name = Some("Марджит".into());
        assert!(content(&s, &c).has_fight());
    }

    /// Доля пройденного не должна делиться на ноль до загрузки парамов.
    #[test]
    fn progress_survives_an_empty_registry() {
        let s = Snapshot::default();
        let c = Config::default();
        let (text, ratio) = content(&s, &c).progress.expect("боссы включены по умолчанию");
        assert_eq!(text, "0/0");
        assert!(ratio.is_finite() && ratio == 0.0, "{ratio}");
    }

    /// Каждая компоновка должна дать положительную ширину на непустых данных:
    /// нулевая означает схлопнутую в точку панель.
    #[test]
    fn every_layout_has_content() {
        let mut s = Snapshot::default();
        s.level = 45;
        s.deaths = 73;
        s.boss_name = Some("Марджит".into());
        let mut c = Config::default();
        for layout in [Layout::A, Layout::B, Layout::D] {
            c.layout = layout;
            let ct = content(&s, &c);
            assert!(!ct.is_empty(), "{layout:?}");
            assert!(ct.has_fight(), "{layout:?}");
        }
    }

    /// Попытка и время боя - такие же метрики, как всё остальное: подпись
    /// капслоком, значение отдельным полем. Раньше это была одна свободная
    /// строка «Попытка 3 · 1:24», и на панели она читалась инородно рядом со
    /// «СМЕРТИ»/«УРОВЕНЬ» (жалоба 2026-08-20).
    #[test]
    fn fight_rows_are_ordinary_metrics() {
        pin_english();
        let mut s = Snapshot::default();
        s.boss_name = Some("Марджит".into());
        s.attempts = 3;
        s.attempt_secs = 84.0;
        let c = Config::default();
        let fight = content(&s, &c).fight;
        assert_eq!(fight.len(), 2, "{fight:?}");
        assert_eq!(fight[0].label, "DEATHS");
        assert_eq!(fight[0].value, "2");
        assert_eq!(fight[1].label, "FIGHT TIME");
        assert_eq!(fight[1].value, "1:24");
        // Каждая строка настраивается своей галочкой, как и обычные метрики.
        let only_timer = Config { show_attempts: false, ..Config::default() };
        assert_eq!(content(&s, &only_timer).fight.len(), 1);
        let neither = Config { show_attempts: false, show_fight_timer: false, ..Config::default() };
        assert!(content(&s, &neither).fight.is_empty());
    }

    /// Подписи метрик локализованы, а язык - глобальный (`i18n`), поэтому
    /// тесты, сравнивающие подпись с литералом, прибивают его к английскому:
    /// файлы локалей в тестах не загружены, и `t()` отдаёт сам ключ.
    /// Значение одно и то же во всех, так что параллельный запуск их не
    /// сталкивает.
    fn pin_english() {
        crate::i18n::apply("en");
    }

    /// при нулевом времени строка не должна появляться (деление на ноль).
    #[test]
    fn kill_and_death_rate_combine_with_playtime() {
        pin_english();
        let mut c = Config::default();
        c.show_boss_kill_rate = true;
        c.show_death_rate = true;
        let mut s = Snapshot::default();
        s.play_time_ms = 0;
        assert!(!content(&s, &c).items.iter().any(|i| i.label == "BOSSES / HOUR"));

        s.play_time_ms = 3_600_000; // час
        s.deaths = 10;
        // boss_count по умолчанию All - считаем оба, чтобы не зависеть от того,
        // какой набор реально читает content().
        s.bosses_named = (5, 165);
        s.bosses_all = (5, 400);
        let items = content(&s, &c).items;
        assert!(items.iter().any(|i| i.label == "BOSSES / HOUR" && i.value == "5.0"), "{items:?}");
        assert!(items.iter().any(|i| i.label == "DEATHS / HOUR" && i.value == "10.0"), "{items:?}");
    }

    /// Попыток на босса - история попыток (файл статистики) делённая на
    /// число убитых, а не на то, что видно прямо сейчас на экране.
    #[test]
    fn avg_attempts_needs_a_kill_to_divide_by() {
        pin_english();
        let mut c = Config::default();
        c.show_avg_attempts = true;
        let mut s = Snapshot::default();
        s.total_attempts = 12;
        s.bosses_named = (0, 165);
        s.bosses_all = (0, 400);
        assert!(!content(&s, &c).items.iter().any(|i| i.label == "ATTEMPTS / BOSS"), "без убитых делить не на что");

        s.bosses_named = (4, 165);
        s.bosses_all = (4, 400);
        let items = content(&s, &c).items;
        assert!(items.iter().any(|i| i.label == "ATTEMPTS / BOSS" && i.value == "3.0"), "{items:?}");
    }

    /// Смертей на боссах - обычная метрика, вне боя тоже видна. А строка боя
    /// считает смерти на текущем боссе с нуля, а не попытки с единицы.
    #[test]
    fn boss_deaths_start_from_zero() {
        pin_english();
        let mut c = Config::default();
        c.show_deaths_on_boss = true;
        let mut s = Snapshot::default();
        s.deaths_on_boss = 3;
        s.attempts = 1;
        let items = content(&s, &c).items;
        assert!(items.iter().any(|i| i.label == "DEATHS ON BOSSES" && i.value == "3"), "{items:?}");

        s.boss_name = Some("Марджит".into());
        let fight = content(&s, &c).fight;
        assert!(fight.iter().any(|i| i.label == "DEATHS" && i.value == "0"), "{fight:?}");

        s.attempts = 4;
        let fight = content(&s, &c).fight;
        assert!(fight.iter().any(|i| i.label == "DEATHS" && i.value == "3"), "{fight:?}");
    }

    /// Стрелка высоты: вверх - босс выше игрока, вниз - ниже. Мелочь вроде
    /// ступеньки под ногами стрелки не рисует вовсе.
    ///
    /// Обе ветки, потому что стрелка есть не всегда: в основном шрифте
    /// её нет, и подмешать системный удаётся не на всякой машине.
    /// Прижатая к краю панель остаётся у края и когда сузилась: иначе после
    /// боя она отходила от края на ширину исчезнувшей строки.
    #[test]
    fn a_panel_pinned_to_the_edge_stays_there() {
        // Свободная панель стоит там, где поставили.
        assert_eq!(place(100.0, 300.0, 1920.0, false), (100.0, false));
        // Расширилась и упёрлась - прижата.
        let (x, pinned) = place(1700.0, 300.0, 1920.0, false);
        assert_eq!((x, pinned), (1620.0, true));
        // Сузилась обратно: край держим, а не возвращаемся на panel_x.
        assert_eq!(place(1700.0, 200.0, 1920.0, pinned), (1720.0, true));
        // Панель шире экрана - к нулю, а не за левый край.
        assert_eq!(place(100.0, 3000.0, 1920.0, false), (0.0, true));
    }

    /// Перенос по словам: панель тянется по самой длинной строке, поэтому
    /// разбивка её и сужает. Слово длиннее лимита не режем - половина слова
    /// читается хуже длинной строки.
    #[test]
    fn a_long_boss_name_wraps_by_words() {
        assert_eq!(wrap_name("Godrick the Grafted", 12), "Godrick the
Grafted");
        assert_eq!(wrap_name("Godrick the Grafted", 0), "Godrick the Grafted", "0 - не переносим");
        assert_eq!(wrap_name("Margit", 3), "Margit", "слово целиком, даже длинное");
        // Двойной босс приезжает двумя строками - каждая переносится сама.
        assert_eq!(wrap_name("Godrick the Grafted
Margit", 12), "Godrick the
Grafted
Margit");
        assert!(
            wrap_name("Malenia, Blade of Miquella", 12).lines().count() > 1,
            "длинное имя обязано разбиться"
        );
    }

    #[test]
    fn height_is_marked_only_when_it_matters() {
        use std::sync::atomic::Ordering;
        assert_eq!(height_mark(0.0), "");
        assert_eq!(height_mark(1.9), "", "ступенька - не этаж");
        assert_eq!(height_mark(-1.9), "");

        ARROWS_BAKED.store(true, Ordering::Relaxed);
        assert_eq!(height_mark(14.0), " \u{2191}14");
        assert_eq!(height_mark(-8.4), " \u{2193}8");

        ARROWS_BAKED.store(false, Ordering::Relaxed);
        assert_eq!(height_mark(14.0), " +14", "без стрелки - знак, а не «?»");
        assert_eq!(height_mark(-8.4), " -8");
    }

}
