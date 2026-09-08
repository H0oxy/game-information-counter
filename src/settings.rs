//! Окно настроек в игре (F7).
//!
//! Вид: слева узкий столбец разделов, справа плитки настроек. Плитки сами
//! раскладываются в один, два или три столбца по ширине окна и каждая ложится
//! в самый низкий столбец, поэтому справа не остаётся пустого места, а на
//! узком окне всё сходится в одну колонку. Сверху поиск по всем разделам
//! сразу, снизу постоянная строка со «Сбросить всё».
//!
//! Единственное место в моде на стоковых виджетах ImGui, а не на ручной
//! отрисовке: панель требует своего вида, окно настроек нет, и повторять ради
//! него весь тулкит было бы работой без цели.
//!
//! Шесть вещей здесь не случайны:
//!
//! 1. **Своя гарнитура фиксированного кегля** (`overlay::settings_font`).
//!    Раньше окно рисовалось шрифтом панели, и подстройка кеглей HUD заодно
//!    растягивала само меню: правишь размер, а под тобой едет интерфейс,
//!    которым правишь. В elden это поймали 2026-07-28, здесь повторили
//!    2026-08-18.
//! 2. **Ползунок пишет в файл, когда его отпустили**, а не каждый кадр
//!    перетаскивания. Иначе одно движение мышью это сотня циклов
//!    «прочитать .ini, переписать, записать», и окно ощутимо застревает.
//! 3. **Кегли применяются тоже по отпусканию**: их изменение пересобирает
//!    атлас шрифтов, а это самая дорогая операция в моде.
//! 4. **Только Latin-1 и кириллица во всём файле, комментарии включая.**
//!    Атлас печётся с `FontGlyphRanges::cyrillic()`, и любой глиф вне этого
//!    набора ImGui рисует знаком вопроса. Длинное тире в подписях и было теми
//!    самыми «?», на которые пожаловались 2026-08-19. Держит тест
//!    `drawn_text_stays_inside_the_baked_glyph_range` в `i18n.rs`.
//! 5. **Шаг ползунка задаёт формат вывода** (`step_of`). ImGui округляет
//!    значение по нему, а стоковый "%.3f" давал тысячные там, где нужны
//!    десятые или целые.
//! 6. **Всё меряется от разрешения экрана** (`scale`): кегль через
//!    `FontGlobalScale`, отступы через `StyleVar`, размер окна и ширина плитки
//!    через тот же коэффициент. На 1080p как было, на 4K вдвое крупнее.
//!
//! Про подписи: если сам элемент понятен по названию, пояснения под ним нет.
//! Плитка называет тему, поэтому подписи внутри неё короткие - длинная в два
//! столбца не помещается, а подписи виджетов ImGui не переносит.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use hudhook::imgui::{Condition, Key, StyleColor, StyleVar, TabItemFlags, Ui, WindowFlags};

use crate::config::{color_to_hex, BossCount, Config, Layout, PanelStyle};
use crate::i18n::{self, t};
use crate::overlay::PadDrag;
use crate::twitch::actions::MAX_HOLD_MS;
use crate::twitch::rewards::{self, Action, RewardEntry};
use crate::twitch::Status;

/// Что изменилось за кадр: пары `(ключ, значение)` для записи в `.ini`.
/// Пусто - файл не трогаем.
pub type Changes = Vec<(&'static str, String)>;

pub struct Outcome {
    pub changes: Changes,
    /// Нажали «Сбросить всё» - конфиг надо вернуть к значениям по умолчанию.
    pub reset: bool,
    /// Нажали «Тестовая покупка»: подделать событие, чтобы посмотреть, как оно
    /// выглядит в игре и в OBS. Единственный способ проверить показ, не имея
    /// ни зрителей, ни баллов канала (их нет у не-Affiliate вовсе).
    pub test_purchase: bool,
    /// Нажали «Проверить» у конкретной награды: исполнить её прямо сейчас,
    /// как будто её купили.
    pub test_reward: Option<u32>,
    /// Список наград изменили - записать файл.
    pub rewards_dirty: bool,
    /// Создать награду на Twitch по этой записи - или, если она там уже есть,
    /// привести её к тому, что настроено здесь.
    pub create_reward: Option<u32>,
    /// Награду удалили из списка - убрать её и с дашборда Twitch. Здесь её
    /// UUID, потому что самой записи к этому моменту уже нет.
    pub delete_on_twitch: Option<String>,
    /// Нажали «Забыть авторизацию»: стереть токен и переподключиться. Нужно,
    /// когда меняется набор прав - выданный токен новых прав не получит.
    pub forget_token: bool,
    /// Нажали «Убрать всех»: снять заспавненных врагов немедленно.
    pub clear_spawns: bool,
    /// Спросить у живой игры, все ли строки таблицы спавна ещё существуют.
    pub audit_spawns: bool,
    /// Нажали «Снять эффекты»: откатить всё, что сейчас висит. Аварийная
    /// кнопка - без неё замедленную игру пришлось бы пережидать.
    pub clear_effects: bool,
    /// Прямо сейчас двигают положение карточки покупки - показать её на
    /// экране, чтобы было видно, куда она встанет.
    pub preview_toast: bool,
    /// Двигают положение оверлея союзников - показать образец.
    pub preview_ally: bool,
    /// Прямо сейчас тянут ползунок блока боя (имя босса, попытка, время):
    /// вне боя его на экране нет, и кегль настраивался бы вслепую.
    pub preview_fight: bool,
    /// То же у подписи над врагом: без врага в кадре её не видно.
    pub preview_tag: bool,
    /// Тянут положение строки босса - показать пару образцов у полоски.
    pub preview_boss: bool,
    /// Нажали «Обновить» в списке зрителей: начать перебор с нуля.
    pub refresh_viewers: bool,
    /// Ник, снятый с «не подписывать»: его надо вернуть в список зрителей.
    pub unblocked: Option<String>,
    /// Показать или спрятать все награды мода на дашборде Twitch.
    pub enable_all_rewards: Option<bool>,
}

/// Всё, что окно читает и меняет за кадр. Собрано в одну структуру, чтобы
/// страницы не таскали по десятку `&mut` каждая: их пять, и при поиске
/// рисуются все сразу.
struct Ctx<'a> {
    /// Коэффициент от разрешения экрана, см. `scale`.
    k: f32,
    c: &'a mut Config,
    changes: Changes,
    rewards: &'a mut Vec<RewardEntry>,
    drag: &'a mut Option<PadDrag>,
    capture: &'a mut Option<u32>,
    status: &'a Status,
    /// Журнал состоявшихся покупок, готовыми строками. Не карточки: те живут
    /// секунды и чистятся каждый кадр.
    log: &'a [String],
    notice: Option<&'a str>,
    connected: bool,
    /// Кто сейчас в списке. Раньше сюда ехало только их число, но список
    /// показывается целиком - с поиском и с ЧС.
    viewers: &'a [String],
    spawned: usize,
    effects_on: usize,
    reset: bool,
    test_purchase: bool,
    test_reward: Option<u32>,
    rewards_dirty: bool,
    create_reward: Option<u32>,
    delete_on_twitch: Option<String>,
    forget_token: bool,
    clear_spawns: bool,
    audit_spawns: bool,
    clear_effects: bool,
    preview_toast: bool,
    preview_ally: bool,
    preview_fight: bool,
    preview_tag: bool,
    preview_boss: bool,
    refresh_viewers: bool,
    unblocked: Option<String>,
    enable_all_rewards: Option<bool>,
}

const PAGE_PANEL: usize = 0;
const PAGE_OBS: usize = 1;
const PAGE_TWITCH: usize = 2;
const PAGE_REWARDS: usize = 3;
const PAGE_OTHER: usize = 4;

/// Названия разделов. Функция, а не константа: язык переключается прямо на
/// живом кадре, в этом же окне.
fn pages() -> [&'static str; 5] {
    [t("Panel"), "OBS", "Twitch", t("Rewards"), t("Other")]
}

/// Выбранный раздел и строка поиска. Статики, а не поля состояния: окно
/// рисуется из одного потока, а тащить эту мелочь через `StreamHud` и
/// сигнатуру `draw` дороже самой мелочи.
static PAGE: AtomicUsize = AtomicUsize::new(0);
static SEARCH: Mutex<String> = Mutex::new(String::new());

/// Насколько подсвечена строка поиска, 0..1. Тянется во времени, как всё
/// остальное в этом окне.
static SEARCH_GLOW: Mutex<f32> = Mutex::new(0.0);

/// Когда нажали «Сбросить всё» в первый раз. Второй клик в течение
/// `RESET_ARMED_SECS` сбрасывает, иначе кнопка возвращается в обычный вид.
static RESET_ARMED: Mutex<Option<std::time::Instant>> = Mutex::new(None);
const RESET_ARMED_SECS: f32 = 4.0;

/// Цвет статуса «на Twitch уже не то, что здесь». Не акцент и не серый:
/// это единственное состояние, которое требует действия, и оно обязано
/// отличаться от обоих спокойных.
const CHANGED: [f32; 4] = [0.93, 0.62, 0.28, 1.0];

/// Какая награда развёрнута. Аккордеон: развёрнутые вперемешку превращают
/// список в простыню, по которой не найти нужную.
static OPEN_REWARD: Mutex<Option<u32>> = Mutex::new(None);

/// Плавность окна редактора награды, 0..1 - появление и уход, а не щелчок
/// (запрос 2026-08-24). Числа те же, что у самого окна настроек: вниз
/// быстрее, чем вверх.
static REWARD_EDITOR_FADE: Mutex<f32> = Mutex::new(0.0);

/// Какую награду показывать в окне редактора, пока оно ещё не отгорело.
/// `OPEN_REWARD` обнуляется в тот же кадр, когда окно закрыли, а рисовать во
/// время затухания всё равно надо чьё-то содержимое.
static REWARD_EDITOR_LAST: Mutex<Option<u32>> = Mutex::new(None);

/// Кому поставить курсор в поле названия - новой награде, на первом же кадре.
static FOCUS_TITLE: Mutex<Option<u32>> = Mutex::new(None);

/// Взведённое удаление награды: чьё и когда. Второй клик в течение
/// `RESET_ARMED_SECS` удаляет, как у «Сбросить всё».
static DELETE_ARMED: Mutex<Option<(u32, std::time::Instant)>> = Mutex::new(None);

/// Состояние тайла награды между кадрами: подсветка под курсором и «живость».
///
/// Раньше тут же жили `h`/`open` для раскрытия прямо в сетке - ушли вместе с
/// ним в `reward_editor_window` (запрос 2026-08-24).
#[derive(Clone, Copy, Default)]
struct RowAnim {
    hov: f32,
    /// 0 - карточки на экране нет, 1 - есть. Новая проявляется от нуля,
    /// удаляемая гаснет до нуля, и только тогда её убирают из списка.
    life: f32,
}

static ROW_H: Mutex<Vec<(u32, RowAnim)>> = Mutex::new(Vec::new());

/// Награда, которую сейчас гасят перед удалением. Одна за раз: удаляют
/// кликом, а не пачкой.
static DYING: Mutex<Option<u32>> = Mutex::new(None);

/// Развёрнут ли гайд подключения. Статик по той же причине, что и остальные
/// в этом файле: это состояние виджета, а не настройка.
static GUIDE_OPEN: AtomicBool = AtomicBool::new(false);

/// Развёрнута ли плитка «Карточка покупки». Настроек в ней полтора десятка, и
/// развёрнутой она вытягивала свой столбец на весь экран (жалоба 2026-09-03).
static TOAST_OPEN: AtomicBool = AtomicBool::new(false);

/// Открыт ли список неубитых боссов и что в нём ищут. Состояние окна, а не
/// настройка: в `.ini` ему делать нечего.
static BOSS_LIST_OPEN: AtomicBool = AtomicBool::new(false);
static BOSS_LIST_HERE: AtomicBool = AtomicBool::new(false);
static BOSS_LIST_KILLED: AtomicBool = AtomicBool::new(false);
/// Показывать только тех, за кого дают воспоминание.
static BOSS_LIST_REMEMBRANCE: AtomicBool = AtomicBool::new(false);
static BOSS_SEARCH: Mutex<String> = Mutex::new(String::new());
static BOSS_LIST_FADE: Mutex<f32> = Mutex::new(0.0);
/// Локации, свёрнутые в списке боссов.
static BOSS_FOLDED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Показан ли Client ID открытым. По умолчанию нет: настройки открывают в
/// игре, то есть на стриме.
static SHOW_CLIENT_ID: AtomicBool = AtomicBool::new(false);

/// Черновик Client ID: поле правится в нём, а в конфиг он едет только по
/// кнопке «Подключить». Раньше подключение начиналось само, стоило вывести
/// курсор из поля, - то есть на каждой опечатке. `None` - поле ещё не трогали,
/// показываем то, что в конфиге.
static CLIENT_ID_DRAFT: Mutex<Option<String>> = Mutex::new(None);

/// Строка поиска и фильтр в списке спавна. Та же причина: это состояние
/// виджета, а не настройка - в `.ini` ему делать нечего.
static SPAWN_SEARCH: Mutex<String> = Mutex::new(String::new());
/// Поиск по списку зрителей. Тоже состояние виджета, не настройка.
/// Что набирают в поле «добавить бота». Состояние виджета, в `.ini` ему
/// делать нечего.
static BOT_DRAFT: Mutex<String> = Mutex::new(String::new());

static VIEWER_SEARCH: Mutex<String> = Mutex::new(String::new());
static SPAWN_BOSSES_ONLY: AtomicBool = AtomicBool::new(false);

/// Просьба выбрать вкладку с клавиатуры (Ctrl+Tab). -1 - просьбы нет.
/// Обычный клик по вкладке сюда не пишет: выбранную хранит сам ImGui.
static WANT_PAGE: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(-1);
/// Просьба поставить курсор в строку поиска (Ctrl+F).
static WANT_FOCUS: AtomicBool = AtomicBool::new(false);
/// Инерционная прокрутка тела: (куда едем, что поставили в прошлый раз).
/// Второе нужно, чтобы отличить свой ход от чужого - полосы прокрутки или
/// смены вкладки.
static SCROLL: Mutex<(f32, f32)> = Mutex::new((0.0, 0.0));
/// Прокрутку в начало: новая вкладка открывается сверху, а не там, где
/// осталась предыдущая.
static SCROLL_HOME: AtomicBool = AtomicBool::new(false);

/// Высота плитки с прошлого кадра. Фон плитки рисуется ДО её содержимого
/// (иначе накрыл бы его сверху), а высота содержимого до отрисовки неизвестна -
/// immediate mode. Состав плитки меняется редко, и один кадр расхождения на
/// глаз не виден.
///
/// Вектор, а не `HashMap`: `HashMap::new` не `const`, а плиток пара десятков.
/// Четвёртое поле - подсветка под курсором, 0..1: она тоже тянется плавно,
/// а состояние между кадрами держать больше негде.
static CARD_H: Mutex<Vec<(usize, &'static str, f32, f32)>> = Mutex::new(Vec::new());

/// Коэффициент интерфейса от высоты экрана. База - 1440p, поэтому на 1080p
/// выходит меньше единицы: запечённые 19 пикселей шрифта в игре читались как
/// «окно огромное» (жалоба 2026-08-20), и весь интерфейс считается от этого
/// же числа. Потолок и пол - чтобы на 4K не раздуло, а на 720p не схлопнуло.
fn scale(ui: &Ui) -> f32 {
    (ui.io().display_size[1] / 1440.0).clamp(0.75, 1.6)
}

#[allow(clippy::too_many_arguments)]
pub fn draw(
    ui: &Ui,
    c: &mut Config,
    open: &mut bool,
    drag: &mut Option<PadDrag>,
    twitch_status: &Status,
    log: &[String],
    rewards: &mut Vec<RewardEntry>,
    viewers: &[String],
    capture: &mut Option<u32>,
    reward_notice: Option<&str>,
    spawned: usize,
    effects_on: usize,
    // 0..1 - окно проявляется по F7 и гаснет при закрытии тем же жестом, что
    // и сама панель HUD. Пока идёт затухание, окно ввода не принимает: клик
    // «сквозь» исчезающее окно попадал бы по виджету, которого уже нет.
    fade: f32,
) -> Outcome {
    let k = scale(ui);
    let accent = c.accent_color;
    let connected = matches!(twitch_status, Status::Connected { .. });
    let mut x = Ctx {
        k,
        c,
        changes: Vec::new(),
        rewards,
        drag,
        capture,
        status: twitch_status,
        log,
        notice: reward_notice,
        connected,
        viewers,
        spawned,
        effects_on,
        reset: false,
        test_purchase: false,
        test_reward: None,
        rewards_dirty: false,
        create_reward: None,
        delete_on_twitch: None,
        forget_token: false,
        clear_spawns: false,
        audit_spawns: false,
        clear_effects: false,
        preview_toast: false,
        preview_ally: false,
        preview_fight: false,
        preview_tag: false,
        preview_boss: false,
        refresh_viewers: false,
        unblocked: None,
        enable_all_rewards: None,
    };
    let mut opened = *open;

    // Кегль всего окна разом, включая дочерние окна и подсказки:
    // `set_window_font_scale` пришлось бы звать в каждом из них по отдельности.
    // Панель и карточки покупок это не трогает - они рисуются своей отрисовкой
    // с явным размером в пикселях.
    unsafe { (*hudhook::imgui::sys::igGetIO()).FontGlobalScale = k };

    // Своя гарнитура на всё окно - см. пункт 1 в шапке модуля.
    let font = crate::overlay::settings_font();
    let pushed = !font.is_null();
    if pushed {
        unsafe { hudhook::imgui::sys::igPushFont(font as *mut _) };
    }

    let _colors = theme(ui, accent, x.c.value_color, x.c.label_color);
    let _vars = metrics(ui, k);
    let _fade = ui.push_style_var(StyleVar::Alpha(fade.clamp(0.0, 1.0)));

    // Клавиши окна. Только когда оно уже проявилось: во время затухания оно и
    // так не принимает ввод.
    let live = fade >= 0.999;
    let mut esc_close = false;
    if live {
        let ctrl = ui.io().key_ctrl;
        if ctrl && ui.is_key_pressed(Key::F) {
            WANT_FOCUS.store(true, Ordering::Relaxed);
        }
        if ctrl && ui.is_key_pressed(Key::Tab) {
            let count = pages().len() as isize;
            let step = if ui.io().key_shift { -1 } else { 1 };
            WANT_PAGE.store((PAGE.load(Ordering::Relaxed) as isize + step).rem_euclid(count), Ordering::Relaxed);
        }
        // Esc не закрывает, пока ждём назначения клавиши награде (им же
        // отменяется сам захват) и пока правят поле ввода: там Esc откатывает
        // набранное, это работа ImGui.
        if ui.is_key_pressed(Key::Escape) && x.capture.is_none() && !ui.is_any_item_active() {
            esc_close = true;
        }
    }

    let ds = ui.io().display_size;
    // Окно висит поверх игры, и занимать пол-экрана ему незачем: треть по
    // ширине и половина по высоте (жалоба «слишком огромное»).
    let min_w = 380.0 * k;
    let min_h = 300.0 * k;
    let mut flags = WindowFlags::empty();
    if !live {
        // Только мышь: NO_INPUTS отобрал бы у окна ещё и фокус, и после
        // проявления оно осталось бы без него - Ctrl+F ставил бы курсор в
        // поле, в которое не печатается.
        flags |= WindowFlags::NO_MOUSE_INPUTS;
    }
    ui.window("Game Information Counter")
        .flags(flags)
        .opened(&mut opened)
        .size(
            [(ds[0] * 0.36).clamp(min_w, ds[0].max(min_w)), (ds[1] * 0.50).clamp(min_h, ds[1].max(min_h))],
            Condition::FirstUseEver,
        )
        .size_constraints([min_w, min_h], [f32::MAX, f32::MAX])
        .build(|| shell(ui, &mut x));

    // Отдельное плавающее окно редактора награды - вне главного, поэтому и
    // рисуется отдельно. Та же тема и тот же кегль (`_colors`/`_vars` ещё не
    // отпущены, шрифт ещё не вытолкнут) - до `PopFont` ниже, иначе редактор
    // выглядел бы стоковым ImGui поверх золотой темы настроек.
    reward_editor_window(ui, &mut x);

    // Сначала масштаб обратно, потом `PopFont`: базовый кегль ImGui считает
    // именно в `SetCurrentFont`, и в обратном порядке всё, что рисуется за
    // окном настроек, доживало бы кадр с нашим множителем.
    unsafe { (*hudhook::imgui::sys::igGetIO()).FontGlobalScale = 1.0 };
    if pushed {
        unsafe { hudhook::imgui::sys::igPopFont() };
    }

    // «Сбросить всё» стирает и Client ID - черновик обязан перечитать конфиг,
    // иначе в поле осталось бы стёртое значение.
    if x.reset {
        *CLIENT_ID_DRAFT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    // Ставится ПОСЛЕ отрисовки, а не до: иначе окно пропало бы на кадр
    // раньше, чем началось затухание, и получился бы щелчок.
    *open = opened && !esc_close;
    Outcome {
        changes: x.changes,
        reset: x.reset,
        test_purchase: x.test_purchase,
        test_reward: x.test_reward,
        rewards_dirty: x.rewards_dirty,
        create_reward: x.create_reward,
        delete_on_twitch: x.delete_on_twitch.take(),
        forget_token: x.forget_token,
        clear_spawns: x.clear_spawns,
        audit_spawns: x.audit_spawns,
        clear_effects: x.clear_effects,
        preview_toast: x.preview_toast,
        preview_ally: x.preview_ally,
        preview_fight: x.preview_fight,
        preview_tag: x.preview_tag,
        preview_boss: x.preview_boss,
        refresh_viewers: x.refresh_viewers,
        unblocked: x.unblocked,
        enable_all_rewards: x.enable_all_rewards,
    }
}

/// Тёмно-золотая тема плиты вместо стокового серого ImGui. Окно красится
/// тем же акцентом, что и сама панель.
/// Тёмный тон акцента - подложки, кнопки, вкладки. Множители подобраны так,
/// чтобы на золоте по умолчанию (`DEB870`) выйти в прежние литералы.
fn shade(c: [f32; 4], k: f32, a: f32) -> [f32; 4] {
    [c[0] * k, c[1] * k, c[2] * k, a]
}

/// Светлый тон акцента - галочка и подсветка края: тянем к белому.
fn tint(c: [f32; 4], k: f32, a: f32) -> [f32; 4] {
    [c[0] + (1.0 - c[0]) * k, c[1] + (1.0 - c[1]) * k, c[2] + (1.0 - c[2]) * k, a]
}

fn theme(ui: &Ui, gold: [f32; 4], text: [f32; 4], disabled: [f32; 4]) -> Vec<hudhook::imgui::ColorStackToken<'_>> {
    // Окно открывают прямо в игре, и сквозь него должно быть видно, что
    // происходит на экране - иначе положение панели настраивается вслепую.
    // 0.96 -> 0.82 -> 0.68 -> 0.72 по прямым просьбам.
    let win_bg = [0.045, 0.045, 0.06, 0.72];
    let colors = [
        (StyleColor::WindowBg, win_bg),
        (StyleColor::PopupBg, win_bg),
        // Дочерние окна (столбец разделов, тело, списки) своего фона не имеют:
        // плитки рисуют его сами, а второй слой затемнения читался бы грязью
        // поверх сцены.
        (StyleColor::ChildBg, [0.0, 0.0, 0.0, 0.0]),
        // Светлая подсветка по краю вместо глухой рамки: тёмная плашка на
        // тёмной сцене иначе сливается, а резкая золотая обводка вокруг
        // каждой галочки читалась как решётка (обе попытки 2026-08-20).
        (StyleColor::Border, tint(gold, 0.85, 0.28)),
        (StyleColor::Text, text),
        (StyleColor::TextDisabled, disabled),
        // Подложка галочек, полей и ползунков. На прозрачном окне их не было
        // видно вовсе (жалоба 2026-08-20). Контраст даёт ТЕМНОТА, а не
        // обводка, но не в чёрноту: глухие плашки читались как дырки.
        (StyleColor::FrameBg, shade(gold, 0.11, 0.88)),
        (StyleColor::FrameBgHovered, shade(gold, 0.19, 0.94)),
        (StyleColor::FrameBgActive, shade(gold, 0.27, 0.98)),
        // Сама галочка/точка - светлее акцента: золото по золоту на золотой
        // же рамке различается плохо.
        (StyleColor::CheckMark, tint(gold, 0.80, 1.0)),
        (StyleColor::SliderGrab, gold),
        (StyleColor::SliderGrabActive, [1.0, 1.0, 1.0, 1.0]),
        // Кнопки заметно светлее плашек: по ним кликают, и они должны
        // читаться как кнопки, а не как подписи.
        (StyleColor::Button, shade(gold, 0.29, 0.9)),
        (StyleColor::ButtonHovered, shade(gold, 0.43, 0.95)),
        (StyleColor::ButtonActive, crate::overlay::with_alpha(gold, 0.7)),
        (StyleColor::Header, crate::overlay::with_alpha(gold, 0.22)),
        (StyleColor::HeaderHovered, crate::overlay::with_alpha(gold, 0.38)),
        (StyleColor::HeaderActive, crate::overlay::with_alpha(gold, 0.55)),
        (StyleColor::Separator, crate::overlay::with_alpha(gold, 0.4)),
        (StyleColor::TitleBg, [0.02, 0.02, 0.03, 1.0]),
        (StyleColor::TitleBgActive, shade(gold, 0.11, 1.0)),
        (StyleColor::TitleBgCollapsed, [0.02, 0.02, 0.03, 1.0]),
        (StyleColor::ScrollbarBg, [0.0, 0.0, 0.0, 0.3]),
        (StyleColor::ScrollbarGrab, crate::overlay::with_alpha(gold, 0.4)),
        (StyleColor::ScrollbarGrabHovered, crate::overlay::with_alpha(gold, 0.6)),
        // Вкладки по умолчанию стокового синего - перекрашиваем под остальное.
        (StyleColor::Tab, shade(gold, 0.11, 0.85)),
        (StyleColor::TabHovered, crate::overlay::with_alpha(gold, 0.5)),
        (StyleColor::TabActive, crate::overlay::with_alpha(gold, 0.34)),
        (StyleColor::TabUnfocused, shade(gold, 0.09, 0.75)),
        (StyleColor::TabUnfocusedActive, crate::overlay::with_alpha(gold, 0.24)),
    ];
    colors.iter().map(|(s, col)| ui.push_style_color(*s, *col)).collect()
}

/// Отступы и скругления - все от `k`, иначе на 4K крупный текст сидел бы в
/// прежних мелких плашках.
fn metrics(ui: &Ui, k: f32) -> Vec<hudhook::imgui::StyleStackToken<'_>> {
    let vars = [
        StyleVar::WindowPadding([10.0 * k, 8.0 * k]),
        StyleVar::FramePadding([6.0 * k, 3.0 * k]),
        StyleVar::ItemSpacing([8.0 * k, 5.0 * k]),
        StyleVar::ItemInnerSpacing([6.0 * k, 4.0 * k]),
        StyleVar::ScrollbarSize(11.0 * k),
        StyleVar::GrabMinSize(10.0 * k),
        StyleVar::WindowRounding(8.0 * k),
        StyleVar::ChildRounding(6.0 * k),
        StyleVar::FrameRounding(3.0 * k),
        StyleVar::TabRounding(4.0 * k),
        StyleVar::WindowBorderSize(1.5),
        // Тонкая, в один пиксель: она обозначает край плашки, а не обводит её.
        StyleVar::FrameBorderSize(1.0),
    ];
    vars.into_iter().map(|v| ui.push_style_var(v)).collect()
}

/// Каркас окна: вкладки разделов сверху, тело и постоянная нижняя строка.
fn shell(ui: &Ui, x: &mut Ctx) {
    let k = x.k;
    // Вкладка сама хранит, какая из них выбрана; `PAGE` только повторяет за
    // ней, чтобы `body` знал, что рисовать.
    // -1, если с клавиатуры вкладку не просили. `swap` - чтобы просьба
    // сработала ровно один раз: иначе `SET_SELECTED` каждый кадр не давал бы
    // переключиться мышью.
    let want = WANT_PAGE.swap(-1, Ordering::Relaxed);
    if let Some(bar) = ui.tab_bar("##pages") {
        for (i, name) in pages().into_iter().enumerate() {
            let mut flags = TabItemFlags::empty();
            if want == i as isize {
                flags |= TabItemFlags::SET_SELECTED;
            }
            // Id вкладки держится за номер, а не за название: иначе смена
            // языка прямо в этом окне сбрасывала бы выбор на первую.
            // `###`, а не `##`: `##` прячет текст только от показа, а id ImGui
            // хеширует по всей строке. При смене языка подписи вкладок
            // менялись, бар видел пять новых вкладок и сбрасывал выбор на
            // первую («переносит на случайную вкладку», жалоба 2026-08-20).
            if let Some(item) = ui.tab_item_with_flags(format!("{name}###p{i}"), None, flags) {
                if PAGE.swap(i, Ordering::Relaxed) != i {
                    // Смена вкладки снимает поиск: пока он не пуст, плитки идут
                    // со всех разделов, и клик по вкладке выглядел бы сломанным.
                    SEARCH.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    // И запускает переход: плитки нового раздела проявятся.
                    SCROLL_HOME.store(true, Ordering::Relaxed);
                }
                item.end();
            }
        }
        bar.end();
    }

    // Нижняя строка не уезжает вместе с настройками: «Сбросить всё» и
    // подсказка про F7 нужны из любого места.
    let footer = ui.frame_height() + ui.text_line_height_with_spacing() + 10.0 * k;
    let avail = ui.content_region_avail();
    let body_h = (avail[1] - footer).max(120.0 * k);
    // Колесо у дочернего окна отключено: его крутит `smooth_scroll`, и вдвоём
    // они спорили бы за одну и ту же прокрутку.
    ui.child_window("##body")
        .size([0.0, body_h])
        .flags(WindowFlags::NO_SCROLL_WITH_MOUSE)
        .build(|| body(ui, x));

    ui.separator();
    // Правый край строки: `content_region_avail` считается от курсора, поэтому
    // берём его ДО первой кнопки, пока курсор стоит в начале строки.
    let line_x = ui.cursor_pos()[0];
    let line_w = ui.content_region_avail()[0];
    // Сброс в два клика: он стирает все настройки разом, и промах мышью по
    // нему стоил бы всей подстройки панели. Второй клик - в течение
    // `RESET_ARMED_SECS`, потом кнопка сама возвращается в обычный вид.
    let armed = RESET_ARMED.lock().unwrap_or_else(|e| e.into_inner()).map_or(false, |t| {
        t.elapsed().as_secs_f32() < RESET_ARMED_SECS
    });
    if armed {
        let ask = ui.push_style_color(StyleColor::Button, crate::overlay::with_alpha(x.c.accent_color, 0.45));
        if button(ui, t("Really reset?")) {
            x.reset = true;
            *RESET_ARMED.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        ask.end();
    } else if button(ui, t("Reset all")) {
        *RESET_ARMED.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
    }
    ui.same_line_with_spacing(0.0, 14.0 * k);
    ui.text_disabled(t("F7 - close"));

    // Состояние жило внизу бокового столбца, а столбца больше нет. Здесь ему и
    // место: это справка, а не управление. Не влезло - не рисуем вовсе, лучше
    // пусто, чем поверх подсказки.
    ui.same_line();
    let at = ui.cursor_pos();
    // Обрезаем по остатку строки, а не прячем целиком: раньше длинный статус
    // либо не влезал, либо не показывался вовсе (жалоба 2026-08-20).
    let room = line_x + line_w - at[0] - 8.0 * k;
    if room > 24.0 * k {
        let status = clip_to(ui, &footer_status(x), room);
        ui.set_cursor_pos([line_x + line_w - ui.calc_text_size(&status)[0], at[1]]);
        ui.text_disabled(status);
    }
}

/// Одна строка состояния для нижней полосы: Twitch, зрители, заспавненные.
fn footer_status(x: &Ctx) -> String {
    let mut parts = vec![status_word(x.status).to_string()];
    if crate::twitch::chat::channel_name(&x.c.twitch_channel).is_some() {
        parts.push(format!("{}: {}", t("viewers"), x.viewers.len()));
    }
    if x.spawned > 0 {
        parts.push(format!("{}: {}", t("in world"), x.spawned));
    }
    parts.join(" \u{b7} ")
}

/// Короткое состояние Twitch для столбца разделов: подробности живут на самой
/// вкладке, здесь помещается пара слов.
fn status_word(status: &Status) -> &'static str {
    match status {
        Status::Disabled => t("Twitch: off"),
        Status::NotConfigured => t("Twitch: no ID"),
        Status::NeedsAuth => t("Twitch: not connected"),
        Status::Connecting => t("Twitch: connecting..."),
        Status::Connected { .. } => t("Twitch: connected"),
        Status::AwaitingDeviceCode { .. } => t("Twitch: enter the code"),
        Status::Error { .. } => t("Twitch: error"),
    }
}

/// Тело окна: поиск и плитки.
fn body(ui: &Ui, x: &mut Ctx) {
    let k = x.k;
    let dt = ui.io().delta_time.clamp(0.0, 0.1);
    smooth_scroll(ui, dt);

    let mut guard = SEARCH.lock().unwrap_or_else(|e| e.into_inner());
    // Ctrl+F: курсор встаёт в строку поиска, и можно сразу печатать.
    // `set_keyboard_focus_here` действует на СЛЕДУЮЩИЙ виджет, поэтому стоит
    // ровно здесь.
    if WANT_FOCUS.swap(false, Ordering::Relaxed) {
        ui.set_keyboard_focus_here();
    }
    ui.set_next_item_width((ui.content_region_avail()[0] - 34.0 * k).max(60.0 * k));
    let _ = ui.input_text("##search", &mut guard).hint(t("Search settings")).build();
    // Подсветка активного поля: рамка акцентом, плавно. Штатная `FrameBgActive`
    // меняет только заливку и на прозрачном окне почти не читается.
    {
        let mut glow = SEARCH_GLOW.lock().unwrap_or_else(|e| e.into_inner());
        *glow = approach(*glow, if ui.is_item_active() { 1.0 } else { 0.0 }, dt, 0.12);
        if *glow > 0.01 {
            let (a, b) = (ui.item_rect_min(), ui.item_rect_max());
            crate::overlay::card_frame(
                [a[0] - 1.0, a[1] - 1.0],
                [b[0] + 1.0, b[1] + 1.0],
                [0.0, 0.0, 0.0, 0.0],
                crate::overlay::with_alpha(x.c.accent_color, 0.75 * *glow),
                4.0 * k,
            );
        }
    }
    ui.same_line();
    // Крестик, а не «Сброс»: строке поиска место, а слово тут лишнее.
    if button(ui, "x") {
        guard.clear();
    }
    let query = guard.trim().to_lowercase();
    drop(guard);

    let page = PAGE.load(Ordering::Relaxed);
    if query.is_empty() {
        page_head(ui, x, page);
    } else {
        ui.dummy([1.0, 4.0 * k]);
    }

    let mut g = Grid::new(ui, k, x.c.accent_color, query, page);
    // При поиске раздел не важен: плитки собираются со всех сразу, а
    // `Grid::wants` отсеивает те, что не подошли.
    let all = !g.query.is_empty();
    if all || page == PAGE_PANEL {
        dimmed(ui, &mut g, !x.c.overlay_enabled, |g| page_panel(ui, g, x));
    }
    if all || page == PAGE_OBS {
        dimmed(ui, &mut g, !x.c.web_enabled, |g| page_obs(ui, g, x));
    }
    if all || page == PAGE_TWITCH {
        page_twitch(ui, &mut g, x);
    }
    if all || page == PAGE_REWARDS {
        page_rewards(ui, &mut g, x);
    }
    if all || page == PAGE_OTHER {
        page_other(ui, &mut g, x);
    }
    g.finish(ui);
    if g.matched == 0 {
        ui.text_disabled(t("Nothing found."));
    }

    // Завеса поверх раздела наград - предупреждение, а не замок: подключение
    // Twitch тут больше не при чём (запрос 2026-09-03). Кнопка «ОК» снимает её
    // навсегда, и в поиске её нет вовсе - там плитки наград идут вперемешку с
    // чужими, накрывать нечем.
    //
    // Настоящего размытия ImGui не умеет: ближайшее - погасить содержимое
    // (`begin_disabled` + `Grid::alpha`) и положить сверху тёмную завесу.
    if !x.c.rewards_notice_seen && g.query.is_empty() && page == PAGE_REWARDS {
        locked_veil(ui, x);
    }
}

/// Прозрачность погашенного содержимого: под завесой наград и у плиток,
/// выключенных главной галочкой раздела.
const LOCKED_ALPHA: f32 = 0.35;

/// Плитки выключенного вывода: не нажимаются и погашены. Родитель - главная
/// галочка в шапке раздела, сама она остаётся живой, иначе включить обратно
/// было бы нечем.
///
/// Двумя способами сразу, потому что они не заменяют друг друга:
/// `begin_disabled` глушит виджеты ImGui и переход по Tab, а фон и рамку
/// плитки рисует наш draw list, до которого стиль ImGui не достаёт.
fn dimmed(ui: &Ui, g: &mut Grid, off: bool, body: impl FnOnce(&mut Grid)) {
    let alpha = g.alpha;
    let _off = ui.begin_disabled(off);
    if off {
        g.alpha *= LOCKED_ALPHA;
    }
    body(g);
    g.alpha = alpha;
}

/// Завеса «наградам нужен Twitch» поверх раздела. Рисуется из `body` уже вне
/// `begin_disabled`, поэтому кнопка на ней нажимается, а всё под ней - нет.
fn locked_veil(ui: &Ui, x: &mut Ctx) {
    let pos = ui.window_pos();
    let size = ui.window_size();
    crate::overlay::card_frame(
        pos,
        [pos[0] + size[0], pos[1] + size[1]],
        [0.03, 0.03, 0.04, 0.72],
        [0.0, 0.0, 0.0, 0.0],
        0.0,
    );

    let title = t("Rewards need a connected Twitch");
    let step1 = t("Set them up now - they start working once Twitch is connected.");
    let lines = [(title, true), (step1, false)];

    // Курсор в координатах окна, а прямоугольник в экранных: середина считается
    // от прокрутки, иначе надпись уезжает вместе с содержимым под завесой.
    let scroll = ui.scroll_y();
    let mut y = scroll + size[1] * 0.5 - 2.0 * ui.text_line_height_with_spacing();
    for (text, accent) in lines {
        let w = ui.calc_text_size(text)[0];
        ui.set_cursor_pos([(size[0] - w) * 0.5, y]);
        if accent {
            ui.text_colored(x.c.accent_color, text);
            y += ui.text_line_height_with_spacing() * 1.6;
        } else {
            ui.text_disabled(text);
            y += ui.text_line_height_with_spacing();
        }
    }

    y += ui.text_line_height_with_spacing() * 0.5;
    let w = ui.calc_text_size(t("OK"))[0] + 40.0 * x.k;
    ui.set_cursor_pos([(size[0] - w) * 0.5, y]);
    if ui.button_with_size(t("OK"), [w, 0.0]) {
        x.c.rewards_notice_seen = true;
        x.changes.push(("rewards_notice_seen", "true".to_string()));
    }
}

/// Инерционная прокрутка тела. ImGui двигает прокрутку рывком на всю
/// ступеньку колеса; здесь колесо копит цель, а прокрутка каждый кадр
/// подтягивается к ней.
///
/// Чужой ход (полоса прокрутки, смена вкладки, клавиши) виден по расхождению с
/// тем, что мы поставили сами: тогда цель принимаем, а не тащим обратно.
fn smooth_scroll(ui: &Ui, dt: f32) {
    let mut st = SCROLL.lock().unwrap_or_else(|e| e.into_inner());
    if SCROLL_HOME.swap(false, Ordering::Relaxed) {
        *st = (0.0, 0.0);
        ui.set_scroll_y(0.0);
        return;
    }
    let cur = ui.scroll_y();
    if (cur - st.1).abs() > 1.0 {
        st.0 = cur;
    }
    let wheel = ui.io().mouse_wheel;
    if wheel != 0.0 && ui.is_window_hovered() {
        st.0 = (st.0 - wheel * ui.frame_height() * 3.0).clamp(0.0, ui.scroll_max_y());
    }
    if (st.0 - cur).abs() < 0.5 {
        return;
    }
    let next = approach(cur, st.0, dt, 0.055);
    ui.set_scroll_y(next);
    st.1 = next;
}

/// Главный выключатель этого вывода, строкой над плитками. Названия раздела
/// тут нет - его несёт сама вкладка. При поиске не рисуется: разделов там
/// пять, и одна галочка врала бы.
fn page_head(ui: &Ui, x: &mut Ctx, page: usize) {
    let k = x.k;
    let toggle = match page {
        PAGE_PANEL => Some(("overlay_enabled", t("Show in game"))),
        PAGE_OBS => Some(("web_enabled", t("Enable the widget"))),
        PAGE_TWITCH => Some(("twitch_enabled", t("Enable the integration"))),
        _ => None,
    };
    if let Some((key, label)) = toggle {
        ui.dummy([1.0, 2.0 * k]);
        let (hit, val) = {
            let v = match key {
                "overlay_enabled" => &mut x.c.overlay_enabled,
                "web_enabled" => &mut x.c.web_enabled,
                _ => &mut x.c.twitch_enabled,
            };
            (ui.checkbox(label, v), *v)
        };
        if hit {
            x.changes.push((key, val.to_string()));
        }
        ui.dummy([1.0, 2.0 * k]);
    }
}

// ---------------------------------------------------------------------------
// Раскладка плиток
// ---------------------------------------------------------------------------

/// Плитки кладутся по столбцам: сколько их, решает ширина тела окна, а каждая
/// новая плитка идёт в самый низкий столбец. Отсюда и «эффективно занимают
/// место»: дыр справа не остаётся, а на узком окне столбец остаётся один.
///
/// Позиция задаётся `set_cursor_pos` ВМЕСТЕ с `indent_by`: ImGui начинает
/// каждую новую строку от текущего отступа окна, и без второго второй элемент
/// плитки уехал бы к левому краю тела.
struct Grid {
    k: f32,
    page: usize,
    /// Уже в нижнем регистре. Пустая - фильтра нет, показываем свой раздел.
    query: String,
    cols: usize,
    col_w: f32,
    full_w: f32,
    gap: f32,
    base_x: f32,
    col_y: Vec<f32>,
    matched: usize,
    accent: [f32; 4],
    /// Прозрачность перехода между вкладками. Стиль ImGui её на нашу
    /// отрисовку не распространяет: плитки рисуются сырым draw list.
    alpha: f32,
    dt: f32,
}

/// Ниже этой ширины плитка перестаёт быть читаемой: подпись плюс ползунок в
/// один ряд просто не помещаются.
const MIN_CARD_W: f32 = 300.0;

/// Запас под тем, что стоит НИЖЕ списка в окне редактора награды (кнопки,
/// подсказки «не создана»/«не подключено», строка рассинхрона). Их высоту
/// заранее не измерить - текст переносится по ширине, - поэтому число, а не
/// расчёт: не хватит, у окна редактора есть своя прокрутка.
///
/// Одно на оба списка (эффекты и спавн): два числа за одно и то же неизбежно
/// разъезжаются, и это уже случилось - у эффектов высота была фиксированной, и
/// список обрезало.
const LIST_FOOTER: f32 = 110.0;
/// Больше трёх столбцов не делаем даже на 4K: глаз теряет строку.
const MAX_COLS: usize = 3;

/// Сколько столбцов помещается в такую ширину. Отдельной функцией ради теста:
/// это и есть вся «адаптивность», и ошибка здесь означала бы плитки, которые
/// вылезают за край окна.
fn column_count(avail: f32, k: f32, gap: f32) -> usize {
    let min_w = MIN_CARD_W * k;
    (((avail + gap) / (min_w + gap)).floor() as usize).clamp(1, MAX_COLS)
}

impl Grid {
    fn new(ui: &Ui, k: f32, accent: [f32; 4], query: String, page: usize) -> Grid {
        let gap = 8.0 * k;
        // Пара пикселей запаса: ширина считается каждый кадр, и на самой
        // границе число столбцов иначе моргало бы туда-сюда из-за полосы
        // прокрутки, которая то появляется, то нет.
        let avail = (ui.content_region_avail()[0] - 2.0).max(120.0);
        let cols = column_count(avail, k, gap);
        let col_w = ((avail - gap * (cols - 1) as f32) / cols as f32).max(120.0);
        let base = ui.cursor_pos();
        Grid {
            k,
            page,
            query,
            cols,
            col_w,
            full_w: avail,
            gap,
            base_x: base[0],
            col_y: vec![base[1]; cols],
            matched: 0,
            accent,
            alpha: 1.0,
            dt: ui.io().delta_time.clamp(0.0, 0.1),
        }
    }

    /// Показывать ли плитку: без поиска - только свой раздел, с поиском -
    /// совпадение по названию или по ключевым словам.
    fn wants(&self, page: usize, title: &str, keys: &str) -> bool {
        if self.query.is_empty() {
            return page == self.page;
        }
        title.to_lowercase().contains(&self.query) || keys.contains(&self.query)
    }

    /// Обычная плитка: в самый низкий столбец.
    fn card(&mut self, ui: &Ui, page: usize, title: &'static str, keys: &str, body: impl FnOnce(f32)) {
        if !self.wants(page, title, keys) {
            return;
        }
        let mut col = 0;
        for i in 1..self.cols {
            if self.col_y[i] < self.col_y[col] - 0.5 {
                col = i;
            }
        }
        let w = self.col_w;
        self.emit(ui, page, title, col, w, body);
    }

    /// Плитка во всю ширину - для списков, которым два столбца только мешают.
    fn wide(&mut self, ui: &Ui, page: usize, title: &'static str, keys: &str, body: impl FnOnce(f32)) {
        if !self.wants(page, title, keys) {
            return;
        }
        let y = self.col_y.iter().copied().fold(0.0_f32, f32::max);
        for v in self.col_y.iter_mut() {
            *v = y;
        }
        let w = self.full_w;
        self.emit(ui, page, title, 0, w, body);
        let bottom = self.col_y[0];
        for v in self.col_y.iter_mut() {
            *v = bottom;
        }
    }

    fn emit(&mut self, ui: &Ui, page: usize, title: &'static str, col: usize, w: f32, body: impl FnOnce(f32)) {
        self.matched += 1;
        let k = self.k;
        let pad = 9.0 * k;
        let x = self.base_x + col as f32 * (self.col_w + self.gap);
        let y = self.col_y[col];
        let inner = w - pad * 2.0;

        let dx = x - self.base_x + pad;
        ui.indent_by(dx);
        ui.set_cursor_pos([x + pad, y + pad]);
        let top = ui.cursor_screen_pos();

        // Высота и подсветка с прошлого кадра, см. `CARD_H`.
        let (h, mut hov) = remembered(page, title);
        let min = [top[0] - pad, top[1] - pad];
        let max = [min[0] + w, min[1] + h];
        if h > 0.0 {
            // Подсветка тянется, а не щёлкает: плиток на экране до шести, и
            // мгновенное переключение между ними мельтешит.
            let over = ui.is_mouse_hovering_rect(min, max);
            hov = approach(hov, if over { 1.0 } else { 0.0 }, self.dt, 0.09);
            let a = self.alpha;
            crate::overlay::card_frame(
                min,
                max,
                [0.10, 0.09, 0.07, (0.45 + 0.14 * hov) * a],
                crate::overlay::with_alpha(self.accent, (0.22 + 0.3 * hov) * a),
                8.0 * k,
            );
        }

        // Свой id на плитку: при поиске рядом оказываются одноимённые плитки
        // разных разделов («Размеры» у панели и у виджета OBS), и без этого
        // ImGui считал бы их виджеты одними и теми же.
        let _id = ui.push_id(format!("card{page}:{title}"));
        let _wrap = ui.push_text_wrap_pos_with_pos(x + w - pad);
        // Ширина по умолчанию - вся плитка. Отсюда её берёт `fit`, вычитая
        // подпись, чтобы та не вылезла в соседний столбец.
        let _iw = ui.push_item_width(inner);

        ui.text_colored(self.accent, title.to_uppercase());
        let rule = ui.cursor_screen_pos();
        crate::overlay::rule_line(rule, inner, crate::overlay::with_alpha(self.accent, 0.3 * self.alpha));
        ui.dummy([1.0, 3.0 * k]);

        body(inner);

        let used = ui.cursor_pos()[1] - y + pad;
        remember(page, title, used, hov);
        ui.unindent_by(dx);
        self.col_y[col] = y + used + self.gap;
    }

    /// Курсор под самый низкий край: без этого окно не знало бы, докуда
    /// прокручиваться, и последняя плитка обрезалась бы.
    fn finish(&mut self, ui: &Ui) {
        let bottom = self.col_y.iter().copied().fold(0.0_f32, f32::max);
        ui.set_cursor_pos([self.base_x, bottom]);
        ui.dummy([1.0, 1.0]);
    }
}

/// Высота плитки и её подсветка с прошлого кадра.
fn remembered(page: usize, title: &'static str) -> (f32, f32) {
    let list = CARD_H.lock().unwrap_or_else(|e| e.into_inner());
    list.iter()
        .find(|(p, name, _, _)| *p == page && *name == title)
        .map(|(_, _, h, hov)| (*h, *hov))
        .unwrap_or((0.0, 0.0))
}

fn remember(page: usize, title: &'static str, h: f32, hov: f32) {
    let mut list = CARD_H.lock().unwrap_or_else(|e| e.into_inner());
    match list.iter_mut().find(|(p, name, _, _)| *p == page && *name == title) {
        Some(slot) => {
            slot.2 = h;
            slot.3 = hov;
        }
        None => list.push((page, title, h, hov)),
    }
}

// Плавный ход к цели живёт в тулките отрисовки: им пользуются и окно
// настроек, и панель, когда меняет размер.
use crate::overlay::approach;

// ---------------------------------------------------------------------------
// Раздел: панель в игре
// ---------------------------------------------------------------------------

fn page_panel(ui: &Ui, g: &mut Grid, x: &mut Ctx) {
    let p = PAGE_PANEL;
    g.card(ui, p, t("What to show"), "боссы смерти на боссах уровень руны время карта ближайший bosses deaths level runes playtime map nearest", |_| {
        check(ui, &mut x.changes, t("Bosses"), "show_bosses", &mut x.c.show_bosses);
        check(ui, &mut x.changes, t("Progress bar"), "show_boss_bar", &mut x.c.show_boss_bar);
        // Свой радиус, не общий с окном списка: там «что осталось в округе»,
        // здесь «куда идти прямо сейчас».
        check(ui, &mut x.changes, t("Nearest boss"), "show_nearest_boss", &mut x.c.show_nearest_boss);
        if x.c.show_nearest_boss {
            slider(ui, &mut x.changes, t("Radius, m"), "nearest_radius", &mut x.c.nearest_radius, 50.0, 3000.0);
        }
        check(ui, &mut x.changes, t("Deaths"), "show_deaths", &mut x.c.show_deaths);
        check(ui, &mut x.changes, t("Deaths on bosses"), "show_deaths_on_boss", &mut x.c.show_deaths_on_boss);
        check(ui, &mut x.changes, t("Viewer kills"), "show_viewer_kills", &mut x.c.show_viewer_kills);
        check(ui, &mut x.changes, t("Level"), "show_level", &mut x.c.show_level);
        check(ui, &mut x.changes, t("Runes"), "show_runes", &mut x.c.show_runes);
        check(ui, &mut x.changes, t("Total runes"), "show_runes_total", &mut x.c.show_runes_total);
        check(ui, &mut x.changes, t("Playtime"), "show_playtime", &mut x.c.show_playtime);
        check(ui, &mut x.changes, "NG+", "show_ng", &mut x.c.show_ng);
        check(ui, &mut x.changes, t("Deathless streak"), "show_deathless", &mut x.c.show_deathless);
        check(ui, &mut x.changes, t("Map explored"), "show_map_explored", &mut x.c.show_map_explored);
    });

    g.card(ui, p, t("During a boss fight"), "босс смертей таймер имя boss deaths timer name", |_| {
        check(ui, &mut x.changes, t("Boss name"), "show_boss_name", &mut x.c.show_boss_name);
        if x.c.show_boss_name {
            // Двойной босс: обычно хватает первого имени, второе - по желанию.
            check(ui, &mut x.changes, t("All boss names"), "show_all_boss_names", &mut x.c.show_all_boss_names);
        }
        check(ui, &mut x.changes, t("Deaths##fight"), "show_attempts", &mut x.c.show_attempts);
        check(ui, &mut x.changes, t("Fight timer"), "show_fight_timer", &mut x.c.show_fight_timer);
    });

    g.card(ui, p, t("Pace"), "боссов в час смертей попыток rate pace hour", |_| {
        check(ui, &mut x.changes, t("Bosses per hour"), "show_boss_kill_rate", &mut x.c.show_boss_kill_rate);
        check(ui, &mut x.changes, t("Deaths per hour"), "show_death_rate", &mut x.c.show_death_rate);
        check(ui, &mut x.changes, t("Attempts per boss"), "show_avg_attempts", &mut x.c.show_avg_attempts);
    });

    g.card(ui, p, t("Which bosses to count"), "боссы знаменатель мини dlc count named all", |_| {
        let mut all = x.c.boss_count == BossCount::All;
        let named = ui.radio_button(t("Named only"), &mut all, false);
        let every = ui.radio_button(t("All"), &mut all, true);
        hint(ui, t("\"All\" counts minibosses and repeats."));
        if named || every {
            x.c.boss_count = if all { BossCount::All } else { BossCount::Named };
            x.changes.push(("boss_count", if all { "all".into() } else { "named".into() }));
        }
    });

    g.card(ui, p, t("Layout"), "компоновка список колонки лента layout", |_| {
        layout_picker(ui, "overlay", &mut x.c.layout, "layout", &mut x.changes);
    });

    g.card(ui, p, t("Frame"), "корпус рамка фон полоса плитка яркость скругление радиус толщина chassis frame border bar radius width", |_| {
        chassis_picker(ui, CHASSIS_OVERLAY, x.c, &mut x.changes);
    });


    g.card(ui, p, t("Position"), "положение позиция координаты position", |w| {
        let accent = x.c.accent_color;
        let (_, released) = crate::overlay::xy_pad(ui, "##panel_xy", &mut x.c.panel_x, &mut x.c.panel_y, accent, x.drag, w);
        // Пишем по отпусканию, как и ползунки: иначе каждое движение мышью это
        // цикл «прочитать .ini, переписать, записать».
        if released {
            x.changes.push(("panel_x", x.c.panel_x.to_string()));
            x.changes.push(("panel_y", x.c.panel_y.to_string()));
        }
        hint(ui, t("Drag the dot. Shift - one axis only."));
        slider(ui, &mut x.changes, t("Horizontal"), "panel_x", &mut x.c.panel_x, 0.0, 3840.0);
        slider(ui, &mut x.changes, t("Vertical"), "panel_y", &mut x.c.panel_y, 0.0, 2160.0);
    });

    g.card(ui, p, t("Sizes"), "размер кегль масштаб шрифт size scale font", |_| {
        // Масштаб как в elden: один ползунок разом двигает и кегли, и вёрстку
        // (`cfg.s()` в overlay.rs). Применяется каждый кадр перетаскивания
        // (живой предпросмотр), а не только по отпусканию.
        let mut sc = x.c.ui_scale;
        let (id, iw) = labeled(ui, t("Overall scale"), "ui_scale");
        ui.set_next_item_width(iw);
        ui.slider_config(id, 0.5, 3.0).display_format("%.1f").build(&mut sc);
        if sc > 0.0 && (sc - x.c.ui_scale).abs() > f32::EPSILON {
            x.c.apply_scale(sc / x.c.ui_scale);
            x.c.ui_scale = sc;
        }
        if ui.is_item_deactivated_after_edit() {
            x.changes.push(("ui_scale", format!("{}", x.c.ui_scale)));
        }
        ui.dummy([1.0, 3.0 * x.k]);
        // Каждый кегль отвечает ровно за одно: раньше «имя босса» двигало
        // заодно счётчик боссов, потому что оба брали `title_size`.
        let k = x.c.ui_scale;
        font_slider(ui, &mut x.changes, t("Labels"), "label_size", &mut x.c.label_size, 8.0, 40.0, k);
        font_slider(ui, &mut x.changes, t("Values"), "value_size", &mut x.c.value_size, 8.0, 48.0, k);
        font_slider(ui, &mut x.changes, t("Boss counter"), "counter_size", &mut x.c.counter_size, 8.0, 56.0, k);
        font_slider(ui, &mut x.changes, t("Boss name"), "boss_name_size", &mut x.c.boss_name_size, 8.0, 40.0, k);
        // Ноль - без переноса. Длинное имя иначе тянет плиту через пол-экрана.
        int_slider(ui, &mut x.changes, t("Name wrap"), "boss_name_wrap", &mut x.c.boss_name_wrap, 0, 40);
        x.preview_fight |= ui.is_item_active();
        font_slider(ui, &mut x.changes, t("Fight rows"), "attempt_size", &mut x.c.attempt_size, 8.0, 48.0, k);
        // Пока тянут - на экране образец боя: вне боя этих строк нет вовсе.
        x.preview_fight |= ui.is_item_active();
        ui.dummy([1.0, 3.0 * x.k]);
        slider(ui, &mut x.changes, t("Letter spacing"), "tracking", &mut x.c.tracking, 0.0, 8.0);
        slider(ui, &mut x.changes, t("Line gap"), "line_gap", &mut x.c.line_gap, 0.0, 24.0);
    });

    g.card(ui, p, t("Color and opacity"), "цвет прозрачность акцент рамка color opacity accent border", |_| {
        slider(ui, &mut x.changes, t("Background"), "panel_opacity", &mut x.c.panel_opacity, 0.0, 1.0);
        ui.dummy([1.0, 3.0 * x.k]);
        color(ui, &mut x.changes, t("Accent"), "accent_color", &mut x.c.accent_color);
        color(ui, &mut x.changes, t("Labels"), "label_color", &mut x.c.label_color);
        color(ui, &mut x.changes, t("Values"), "value_color", &mut x.c.value_color);
    });

}

// ---------------------------------------------------------------------------
// Раздел: виджет в OBS
// ---------------------------------------------------------------------------

/// Виджет в OBS живёт поверх своей сцены, а не поверх игры, поэтому
/// прозрачность, масштаб и кегли у него свои. Общими остаются только цвета и
/// набор метрик: разводить и их было бы уже не настройкой, а вторым модом.
fn page_obs(ui: &Ui, g: &mut Grid, x: &mut Ctx) {
    let p = PAGE_OBS;
    g.card(ui, p, t("Addresses for OBS"), "obs адрес источник браузер browser source url", |_| {
        let port = x.c.web_port;
        copyable(ui, x.c, &format!("http://127.0.0.1:{port}/hud"), t("stats"));
        copyable(ui, x.c, &format!("http://127.0.0.1:{port}/toasts"), t("viewer purchases"));
    });

    g.card(ui, p, t("Layout"), "компоновка список колонки лента layout obs", |_| {
        layout_picker(ui, "web", &mut x.c.web_layout, "web_layout", &mut x.changes);
    });

    g.card(ui, p, t("Frame"), "корпус рамка фон полоса плитка яркость скругление радиус толщина chassis frame border bar radius width obs", |_| {
        chassis_picker(ui, CHASSIS_WEB, x.c, &mut x.changes);
    });

    g.card(ui, p, t("Sizes"), "размер кегль масштаб шрифт size scale font obs", |_| {
        slider(ui, &mut x.changes, t("Overall scale"), "web_scale", &mut x.c.web_scale, 0.5, 3.0);
        ui.dummy([1.0, 3.0 * x.k]);
        slider(ui, &mut x.changes, t("Labels"), "web_label_size", &mut x.c.web_label_size, 8.0, 40.0);
        slider(ui, &mut x.changes, t("Values"), "web_value_size", &mut x.c.web_value_size, 8.0, 48.0);
        slider(ui, &mut x.changes, t("Boss counter"), "web_counter_size", &mut x.c.web_counter_size, 8.0, 56.0);
        slider(ui, &mut x.changes, t("Boss name"), "web_boss_name_size", &mut x.c.web_boss_name_size, 8.0, 40.0);
        x.preview_fight |= ui.is_item_active();
        slider(ui, &mut x.changes, t("Fight rows"), "web_attempt_size", &mut x.c.web_attempt_size, 8.0, 48.0);
        x.preview_fight |= ui.is_item_active();
        ui.dummy([1.0, 3.0 * x.k]);
        slider(ui, &mut x.changes, t("Letter spacing"), "web_tracking", &mut x.c.web_tracking, 0.0, 8.0);
        slider(ui, &mut x.changes, t("Line gap"), "web_line_gap", &mut x.c.web_line_gap, 0.0, 24.0);
    });

    g.card(ui, p, t("Purchase card"), "карточка покупка уведомление размер ширина корпус рамка радиус toast card size width chassis border radius obs", |_| {
        // Карточки в OBS - отдельный источник (/toasts), и размер у них свой:
        // они стоят в углу сцены, а не рядом со статистикой.
        slider(ui, &mut x.changes, t("Labels"), "web_toast_label_size", &mut x.c.web_toast_label_size, 8.0, 40.0);
        slider(ui, &mut x.changes, t("Title"), "web_toast_value_size", &mut x.c.web_toast_value_size, 8.0, 48.0);
        slider(ui, &mut x.changes, t("Max width"), "web_toast_width", &mut x.c.web_toast_width, 120.0, 600.0);
        group(ui, x.c, t("FRAME"));
        chassis_picker(ui, CHASSIS_WEB_TOAST, x.c, &mut x.changes);
        if x.c.web_toast_style != crate::config::PanelStyle::Bare {
            slider(ui, &mut x.changes, t("Background"), "web_toast_opacity", &mut x.c.web_toast_opacity, 0.0, 1.0);
        }
    });

    g.card(ui, p, t("Color and opacity"), "цвет прозрачность рамка color opacity border obs", |_| {
        slider(ui, &mut x.changes, t("Background"), "web_opacity", &mut x.c.web_opacity, 0.0, 1.0);
        hint(ui, t("Colors are shared with the in-game panel."));
    });
}

// ---------------------------------------------------------------------------
// Раздел: Twitch
// ---------------------------------------------------------------------------

/// Подключение, никнеймы и история покупок. `twitch_client_id` подхватывается
/// только при запуске сетевого потока - как `web_port`, читается один раз. Про
/// это написано прямым текстом, иначе «вставил ID, ничего не произошло»
/// выглядит поломкой.
fn page_twitch(ui: &Ui, g: &mut Grid, x: &mut Ctx) {
    let p = PAGE_TWITCH;
    g.card(ui, p, t("Connection"), "twitch client id токен авторизация connect token auth", |w| {
        let shown = SHOW_CLIENT_ID.load(Ordering::Relaxed);
        // Место под кнопки считаем по их тексту, а не на глаз: русские и
        // английские подписи разной длины, и на глазок кнопка вылезала за
        // плитку (жалоба 2026-08-20). Место под ОБЕ резервируется всегда,
        // даже когда «Подключить» погашена: иначе поле дёргалось бы в ширине,
        // стоило её включить.
        let show_w = ui
            .calc_text_size(t("Show"))[0]
            .max(ui.calc_text_size(t("Hide"))[0])
            + 20.0 * x.k;
        let connect_w = ui.calc_text_size(t("Connect"))[0] + 20.0 * x.k;
        let mut draft = CLIENT_ID_DRAFT.lock().unwrap_or_else(|e| e.into_inner());
        let buf = draft.get_or_insert_with(|| x.c.twitch_client_id.clone());
        // В два столбца плитка узкая, и поле схлопывалось до огрызка, в
        // который не влезал даже кусок из тридцати символов Client ID (жалоба
        // 2026-08-24). Не помещается - кнопки уезжают строкой ниже, а поле
        // забирает всю ширину плитки.
        let room = w - show_w - connect_w - 18.0 * x.k;
        let inline = room >= 150.0 * x.k;
        ui.set_next_item_width(if inline { room } else { w });
        // Enter в поле равносилен кнопке: набрал и подтвердил, не тянясь мышью.
        let entered = ui
            .input_text("##client_id", buf)
            .password(!shown)
            .hint("Client ID")
            .enter_returns_true(true)
            .build();
        if inline {
            ui.same_line();
        }
        if button(ui, if shown { t("Hide") } else { t("Show") }) {
            SHOW_CLIENT_ID.store(!shown, Ordering::Relaxed);
        }
        // Подтверждение кнопкой, а не подключение на лету: сетевой поток
        // читает Client ID каждый круг, и без кнопки в Twitch уходила бы
        // каждая промежуточная буква (жалоба 2026-08-22).
        //
        // Кнопка одна на оба дела - применить набранное и начать авторизацию, -
        // и стоит В ОДНОЙ СТРОКЕ с полем: строкой ниже она «убегала слишком
        // низко от поля» (жалоба 2026-08-22). Делать нечего - погашена, а не
        // спрятана: пропадающая кнопка двигала бы всё под собой.
        let pending = buf.trim() != x.c.twitch_client_id.trim();
        let idle = pending || matches!(x.status, Status::NeedsAuth);
        ui.same_line();
        let press = {
            let _off = ui.begin_disabled(!idle);
            button(ui, t("Connect"))
        };
        if idle && (press || entered) {
            if pending {
                x.c.twitch_client_id = buf.trim().to_string();
                x.changes.push(("twitch_client_id", crate::secret::protect(&x.c.twitch_client_id)));
                // «Подключить» и значит подключить: галочку раздела включаем
                // сами, иначе введённый ID просто лежал бы без дела.
                if !x.c.twitch_client_id.is_empty() && !x.c.twitch_enabled {
                    x.c.twitch_enabled = true;
                    x.changes.push(("twitch_enabled", "true".to_string()));
                }
            }
            // Разрешаем новую авторизацию: без этого сетевой поток ждал бы
            // кнопку, а кнопка только что нажата.
            if !x.c.twitch_client_id.trim().is_empty() {
                crate::twitch::request_connect();
            }
        }
        drop(draft);
        link(ui, x.c, "https://dev.twitch.tv/console/apps");

        ui.dummy([1.0, 4.0 * x.k]);
        draw_status(ui, x.c, x.status);
        ui.dummy([1.0, 4.0 * x.k]);
        if button(ui, t("Forget the authorization")) {
            x.forget_token = true;
        }
        // Свёрнут по умолчанию: нужен один раз в жизни, а место занимал бы
        // всегда. Отдельной плиткой он раздувал раздел вдвое.
        //
        // Кнопка, а НЕ `header`: сворачиваемый заголовок ImGui растягивается
        // на всю ширину окна, а не плитки, и в два столбца полоса заголовка
        // шла через весь интерфейс (жалоба 2026-08-20). Ширина кнопки идёт по
        // её тексту, этой болезни у неё нет.
        ui.dummy([1.0, 3.0 * x.k]);
        let open = GUIDE_OPEN.load(Ordering::Relaxed);
        let caption = format!("{} {}", t("How to connect"), if open { "-" } else { "+" });
        if button(ui, caption) {
            GUIDE_OPEN.store(!open, Ordering::Relaxed);
        }
        if open {
            connection_guide(ui, x.c);
        }
    });

    // Откуда берутся имена: канал, источник, список, ЧС.
    g.card(ui, p, t("Viewers"), "зрители канал чат список поиск чс блок источник viewers channel chat list search block source", |w| {
        section_viewers(ui, x, w);
    });

    // Как выглядит подпись. Своей плиткой, а не секцией внутри «Зрителей»
    // (запрос 2026-09-07): настраивают её отдельно от источника имён.
    g.card(ui, p, t("Nicknames"), "ник никнейм подпись реплика враги боссы размер цвет nickname label say enemies bosses size color", |_| {
        section_nicknames(ui, x);
    });

    // Гаснет вместе с интеграцией. Подключение, зрители и никнеймы - нет: чат
    // читается анонимно, и им хватает ссылки на канал, без Client ID и без
    // авторизации.
    dimmed(ui, g, !x.c.twitch_enabled, |g| {
    g.card(ui, p, t("Recent purchases"), "покупки история баллы purchases history points", |w| {
        if x.log.is_empty() {
            hint(ui, t("nothing yet"));
            return;
        }
        ui.child_window("##purchases").size([w, 120.0 * x.k]).build(|| {
            // Новые сверху: старые уезжают вниз сами, скроллить за ними не надо.
            for line in x.log.iter().rev() {
                ui.text(line);
            }
        });
    });
    });
}

/// Краткий гайд «как подключить Twitch»: ровно те два места, где всё ломается -
/// Client Type обязан быть Public, а баллы канала есть только у Affiliate и
/// Partner.
fn connection_guide(ui: &Ui, c: &Config) {
    let step = |n: u32, text: &str| {
        ui.text_colored(c.accent_color, format!("{n}."));
        ui.same_line();
        ui.text_wrapped(text);
    };
    step(1, t("Open dev.twitch.tv/console/apps and register an application. Any name works."));
    link(ui, c, "https://dev.twitch.tv/console/apps/create");
    step(2, t("Fill in the application fields:"));
    // Адрес отдельной строкой и кликом в буфер: набирать его руками, сидя в
    // игре, худшее из возможного, а в сплошной строке он и не читался.
    copyable(ui, c, "http://localhost", "OAuth Redirect URL");
    // Название поля и то, что в него вставляют, разного цвета - иначе строка
    // читается как одно предложение и непонятно, что именно копировать
    // (запрос 2026-09-03). Тот же язык, что у `copyable` строкой выше.
    let field = |name: &str, value: &str| {
        ui.text_disabled(name);
        ui.same_line();
        ui.text_colored(c.accent_color, value);
    };
    field("Category", "Game Integration");
    field("Client Type", "Public");
    hint(ui, t("Without Public the device login does not work at all."));
    step(3, t("Copy the application Client ID into the field above and enable the integration."));
    step(4, t("A code appears - enter it on twitch.tv/activate under your channel."));
    step(5, t("Rewards are created on the Rewards page with \"Create on Twitch\", or by hand on the dashboard - the mod matches them by title."));
}

fn draw_status(ui: &Ui, c: &Config, status: &Status) {
    ui.text_disabled(t("Status"));
    match status {
        Status::Disabled => ui.text_disabled(t("disabled")),
        Status::NotConfigured => ui.text_disabled(t("no Client ID entered")),
        // Кнопка «Подключить» одна и живёт у поля Client ID - здесь только
        // объяснение, почему ничего не происходит.
        Status::NeedsAuth => ui.text(t("press \"Connect\" and a code will appear")),
        Status::Connecting => ui.text(t("connecting...")),
        Status::Connected { login } => {
            let who = if login.is_empty() { String::new() } else { format!(": {login}") };
            ui.text_colored(c.accent_color, format!("{}{who}", t("connected")));
        }
        Status::AwaitingDeviceCode { user_code, verification_uri } => {
            ui.text(t("Enter this code on the site:"));
            ui.text_colored(c.accent_color, user_code);
            // Скопировать код мышью из игры нельзя, поэтому дублируем в буфер.
            ui.same_line_with_spacing(0.0, 12.0);
            if button(ui, t("Copy")) {
                ui.set_clipboard_text(user_code);
            }
            link(ui, c, verification_uri);
        }
        Status::Error { message, retry_in_secs } => {
            // Длинное сообщение переносим по ширине плитки, а не режем: причина
            // отказа единственное, по чему это вообще можно чинить.
            ui.text_colored([0.9, 0.5, 0.4, 1.0], t("not connected"));
            ui.text_wrapped(message);
            ui.text_disabled(format!("{} {retry_in_secs}", t("next try in, s:")));
        }
    }
}

/// Откуда берётся список зрителей. Разница большая: Helix отдаёт всех сразу,
/// анонимный чат - только тех, кто пишет, то есть десятки имён за полчаса.
fn chatters_source(ui: &Ui, c: &Config) {
    use crate::config::ViewerSource;
    // При выборе руками показываем выбранное, а не то, что последним ответило
    // по сети: иначе строка врала бы («интеграция», хотя берём только чат).
    let via_app = match c.viewers_source {
        ViewerSource::App => true,
        ViewerSource::Chat => false,
        ViewerSource::Auto => crate::twitch::CHATTERS_VIA_HELIX.load(Ordering::Relaxed),
    };
    let denied = crate::twitch::CHATTERS_DENIED.load(Ordering::Relaxed);

    ui.text_disabled(t("Source:"));
    ui.same_line();
    if via_app {
        // Акцентом, а не серым: это хорошее состояние, и по нему видно, что
        // авторизация реально что-то дала.
        ui.text_colored(c.accent_color, t("app integration"));
        hint(ui, t("everyone at once, lurkers included"));
        return;
    }
    ui.text(t("channel link"));
    hint(ui, t("only those who type in chat"));
    if denied {
        // Право зашито в уже выданный токен - обновиться само оно не может.
        hint(ui, t("The full list needs the integration: \"Forget the authorization\", then log in again"));
    }
}

/// Список зрителей: откуда он берётся, кто в нём есть и кого в нём видеть не
/// надо.
///
/// ЧС живёт в `.ini` строкой через запятую, а не отдельным файлом: список
/// короткий, правится мышкой и по смыслу это такая же настройка, как любая
/// другая.
fn section_viewers(ui: &Ui, x: &mut Ctx, w: f32) {
    use crate::config::ViewerSource;
    // Канал живёт здесь, а не в плитке никнеймов: он и есть источник имён, а
    // подпись над врагом - только их показ.
    let (id, iw) = labeled(ui, t("Channel"), "channel");
    ui.set_next_item_width(iw);
    let _ = ui.input_text(id, &mut x.c.twitch_channel).build();
    if ui.is_item_deactivated_after_edit() {
        let name = x.c.twitch_channel.clone();
        x.changes.push(("twitch_channel", name));
    }
    match crate::twitch::chat::channel_name(&x.c.twitch_channel) {
        Some(name) => hint(ui, &format!("@{name}")),
        None if x.c.twitch_channel.trim().is_empty() => {
            hint(ui, t("Channel name or a link to it."));
        }
        None => {
            let col = ui.push_style_color(StyleColor::Text, [0.9, 0.5, 0.4, 1.0]);
            ui.text_wrapped(t("does not look like a channel"));
            col.end();
        }
    }

    group(ui, x.c, t("SOURCE"));
    // `Auto` - как было всегда: интеграция, пока отвечает, иначе чат. Руками
    // выбирают, когда нужно именно одно из двух.
    for (variant, label) in [
        (ViewerSource::Auto, t("Automatic")),
        (ViewerSource::App, t("Integration")),
        (ViewerSource::Chat, t("Channel link")),
    ] {
        let mut chosen = x.c.viewers_source == variant;
        if ui.radio_button(format!("{label}###src_{}", variant.as_key()), &mut chosen, true)
            && x.c.viewers_source != variant
        {
            x.c.viewers_source = variant;
            x.changes.push(("viewers_source", variant.as_key().to_string()));
        }
    }
    chatters_source(ui, x.c);

    let mut search = VIEWER_SEARCH.lock().unwrap_or_else(|e| e.into_inner());
    let needle = search.to_lowercase();
    let shown: Vec<String> = x
        .viewers
        .iter()
        .filter(|v| needle.is_empty() || v.to_lowercase().contains(&needle))
        .take(CHIP_CAP)
        .cloned()
        .collect();
    // Поиск показывает, сколько из скольких: на людном канале «(1843)» само по
    // себе не говорит, нашлось ли что-нибудь.
    let count = match needle.is_empty() {
        true => x.viewers.len().to_string(),
        false => format!("{} / {}", shown.len(), x.viewers.len()),
    };
    group(ui, x.c, &format!("{} ({count})", t("LIST")));

    ui.set_next_item_width(w * 0.6);
    let _ = ui.input_text("##viewer_search", &mut search).hint(t("Search")).build();
    ui.same_line_with_spacing(0.0, 12.0);
    if button(ui, t("Refresh")) {
        x.refresh_viewers = true;
    }

    // Своё окно со скроллом: зрителей бывает две тысячи, а плитка должна
    // остаться плиткой.
    let mut block: Option<String> = None;
    ui.child_window("##viewer_list").size([w * 0.98, 150.0 * x.k]).build(|| {
        if shown.is_empty() {
            ui.text_disabled(t("empty"));
        }
        if let Some(i) = chips(ui, "vw", &shown, x.viewers.len()) {
            block = Some(shown[i].to_lowercase());
        }
    });
    hint(ui, t("Click a nickname to stop using it."));
    if let Some(nick) = block {
        x.c.viewer_block.push(nick);
        let list = x.c.viewer_block.join(",");
        x.changes.push(("viewer_block", list));
    }

    bot_list_ui(ui, x, w);

    // Ниже - только ЧС, и пустым он места не занимает: список пополняется
    // кликом по нику выше, а не здесь.
    if x.c.viewer_block.is_empty() {
        return;
    }
    group(ui, x.c, &format!("{} ({})", t("NEVER LABELED"), x.c.viewer_block.len()));
    // Красный тон, а не общий: это не «ещё один список зрителей», а изъятые.
    let hot = [
        ui.push_style_color(StyleColor::Button, [0.36, 0.15, 0.13, 0.85]),
        ui.push_style_color(StyleColor::ButtonHovered, [0.52, 0.20, 0.17, 0.92]),
    ];
    let mut back: Option<usize> = None;
    let list: Vec<String> = x.c.viewer_block.clone();
    ui.child_window("##viewer_block").size([w * 0.98, 70.0 * x.k]).build(|| {
        back = chips(ui, "bl", &list, list.len());
    });
    drop(hot);
    hint(ui, t("Click to bring it back."));
    if let Some(i) = back {
        // Возвращаем в список ровно его, а не «Обновить»: тот заводит заново
        // весь `NicknameAssigner`, то есть снимает имена со ВСЕХ врагов в
        // кадре и сбрасывает кулдауны (жалоба 2026-09-07). Вернуть одного
        // дешевле, чем начать перебор с нуля.
        x.unblocked = Some(x.c.viewer_block.remove(i));
        let list = x.c.viewer_block.join(",");
        x.changes.push(("viewer_block", list));
    }
}

/// Боты и сервисы: встроенный список плюс свои, правится прямо тут.
///
/// В `.ini` уезжают только отклонения (`viewer_bots_off` /
/// `viewer_bots_extra`), поэтому боты, добавленные в новой версии мода,
/// доезжают и до тех, кто список уже правил.
fn bot_list_ui(ui: &Ui, x: &mut Ctx, w: f32) {
    let bots = crate::twitch::chat::bot_list(&x.c.viewer_bots_off, &x.c.viewer_bots_extra);
    group(ui, x.c, &format!("{} ({})", t("BOTS"), bots.len()));

    let mut add = BOT_DRAFT.lock().unwrap_or_else(|e| e.into_inner());
    ui.set_next_item_width(w * 0.6);
    let entered = ui
        .input_text("##bot_add", &mut add)
        .hint(t("Add a nickname"))
        .enter_returns_true(true)
        .build();
    ui.same_line_with_spacing(0.0, 12.0);
    if (button(ui, t("Add")) || entered) && !add.trim().is_empty() {
        let nick = crate::twitch::chat::squash(&add);
        // Снятый обратно в боты возвращается снятием пометки, а не второй
        // записью: иначе он лежал бы в обоих списках сразу.
        x.c.viewer_bots_off.retain(|o| crate::twitch::chat::squash(o) != nick);
        let known = x.c.viewer_bots_extra.iter().any(|e| crate::twitch::chat::squash(e) == nick);
        if !nick.is_empty() && !known && !crate::twitch::chat::is_builtin_bot(&nick) {
            x.c.viewer_bots_extra.push(nick);
        }
        push_bot_lists(x);
        add.clear();
    }
    drop(add);

    let mut drop_bot: Option<String> = None;
    ui.child_window("##bot_list").size([w * 0.98, 110.0 * x.k]).build(|| {
        if let Some(i) = chips(ui, "bot", &bots, bots.len()) {
            drop_bot = Some(bots[i].clone());
        }
    });
    hint(ui, t("Click to allow it back."));

    if let Some(nick) = drop_bot {
        x.c.viewer_bots_extra.retain(|e| crate::twitch::chat::squash(e) != nick);
        if crate::twitch::chat::is_builtin_bot(&nick) {
            x.c.viewer_bots_off.push(nick.clone());
        }
        push_bot_lists(x);
        // Тем же путём, что и снятие с ЧС: вернуть одного, а не заводить
        // заново весь перебор имён.
        x.unblocked = Some(nick);
    }
}

/// Обе половины списка пишутся вместе: правка одной почти всегда трогает и
/// вторую.
fn push_bot_lists(x: &mut Ctx) {
    x.changes.push(("viewer_bots_off", x.c.viewer_bots_off.join(",")));
    x.changes.push(("viewer_bots_extra", x.c.viewer_bots_extra.join(",")));
}

/// Сколько чипов рисуем за раз. Зрителей бывает две тысячи, и мерить каждый
/// ник каждый кадр незачем: остальных находит поиск, а сколько их - говорит
/// последний чип.
const CHIP_CAP: usize = 200;

/// Ники потоком: чип по ширине текста, ряд набирается, пока влезает.
///
/// Кнопка, а не текст с крестиком: чип и есть действие, и промахнуться по
/// нему нельзя. Возвращает индекс кликнутого.
///
/// `total` - сколько их всего: хвост сверх `CHIP_CAP` показывается нерабочим
/// чипом «+N», иначе список молча врал бы о составе.
fn chips(ui: &Ui, id: &str, items: &[String], total: usize) -> Option<usize> {
    let pad = ui.clone_style().frame_padding[0] * 2.0;
    let gap = 6.0;
    let room = ui.content_region_avail()[0];
    let mut line = 0.0;
    let mut hit = None;
    for (i, nick) in items.iter().enumerate() {
        // Ник шире окна режем: иначе чип уводит содержимое в горизонтальный
        // скролл, а плитка узкая.
        let nick = clip_to(ui, nick, room - pad);
        let cw = ui.calc_text_size(&nick)[0] + pad;
        if line > 0.0 && line + gap + cw <= room {
            ui.same_line_with_spacing(0.0, gap);
            line += gap + cw;
        } else {
            line = cw;
        }
        if ui.button_with_size(format!("{nick}###{id}_{i}"), [cw, 0.0]) {
            hit = Some(i);
        }
    }
    if total > items.len() {
        let rest = format!("+{}", total - items.len());
        if line > 0.0 {
            ui.same_line_with_spacing(0.0, gap);
        }
        ui.text_disabled(&rest);
    }
    hit
}

/// Как выглядит подпись над врагом. Нижняя половина плитки «Зрители»: имена
/// берутся из чата, который настроен строкой выше, и разводить это по двум
/// плиткам значило прыгать между ними (запрос 2026-09-03).
///
/// От подключения Twitch не зависит вовсе - анонимному чату хватает ссылки на
/// канал.
fn section_nicknames(ui: &Ui, x: &mut Ctx) {
    check(ui, &mut x.changes, t("Show nicknames"), "enemy_tags", &mut x.c.enemy_tags);
    if !x.c.enemy_tags {
        return;
    }
    // Подписывать некем - видно сразу, а не после долгого «почему ничего не
    // появляется». Канал задаётся в плитке «Зрители».
    if x.c.twitch_channel.trim().is_empty() || x.viewers.is_empty() {
        hint(ui, t("No viewers yet - the enemies bought by them are still labeled."));
    }

    // Кому вешать подпись. Врагов и боссов рисуют разные пути (тег игры
    // против своей проекции), поэтому и галочки две.
    check(ui, &mut x.changes, t("On enemies"), "enemy_tags_mobs", &mut x.c.enemy_tags_mobs);
    check(ui, &mut x.changes, t("On bosses"), "enemy_tags_bosses", &mut x.c.enemy_tags_bosses);

    group(ui, x.c, t("LABEL"));
    if x.c.enemy_tags_mobs {
        slider(ui, &mut x.changes, t("Offset X"), "enemy_tag_offset_x", &mut x.c.enemy_tag_offset_x, -200.0, 200.0);
        x.preview_tag |= ui.is_item_active();
        slider(ui, &mut x.changes, t("Offset Y"), "enemy_tag_height", &mut x.c.enemy_tag_height, -100.0, 160.0);
        x.preview_tag |= ui.is_item_active();
    }
    slider(ui, &mut x.changes, t("Size"), "enemy_tag_size", &mut x.c.enemy_tag_size, 8.0, 40.0);
    // Образец подписи в центре экрана: живого врага под настройку не найти.
    x.preview_tag |= ui.is_item_active();
    color(ui, &mut x.changes, t("Color"), "enemy_tag_color", &mut x.c.enemy_tag_color);
    hint(ui, t("Offsets are given for 1080p."));

    group(ui, x.c, t("CHAT MESSAGE"));
    // Чужой текст на экране стрима - решает стример, поэтому галочка своя, а
    // не часть «Показывать никнеймы».
    check(ui, &mut x.changes, t("Show the message"), "enemy_tag_say", &mut x.c.enemy_tag_say);
    hint(
        ui,
        t("The viewer's last chat message goes under their nickname."),
    );
    if x.c.enemy_tag_say {
        slider(ui, &mut x.changes, t("Message X"), "enemy_say_offset_x", &mut x.c.enemy_say_offset_x, -200.0, 200.0);
        x.preview_tag |= ui.is_item_active();
        slider(ui, &mut x.changes, t("Message Y"), "enemy_say_offset_y", &mut x.c.enemy_say_offset_y, -120.0, 120.0);
        x.preview_tag |= ui.is_item_active();
        slider(ui, &mut x.changes, t("Seconds on screen##say"), "enemy_say_secs", &mut x.c.enemy_say_secs, 1.0, 60.0);
        x.preview_tag |= ui.is_item_active();
    }

    group(ui, x.c, t("BOSSES"));
    if x.c.enemy_tags_bosses {
        // Место над головой считает проекция, эти два - только доводка, как у
        // обычных подписей.
        slider(ui, &mut x.changes, t("Boss X"), "boss_tag_offset_x", &mut x.c.boss_tag_offset_x, -300.0, 300.0);
        x.preview_boss |= ui.is_item_active();
        slider(ui, &mut x.changes, t("Boss Y"), "boss_tag_offset_y", &mut x.c.boss_tag_offset_y, -300.0, 300.0);
        x.preview_boss |= ui.is_item_active();
        hint(ui, t("The label sits above the boss's head."));
    }
}

// ---------------------------------------------------------------------------
// Раздел: награды
// ---------------------------------------------------------------------------


fn page_rewards(ui: &Ui, g: &mut Grid, x: &mut Ctx) {
    let p = PAGE_REWARDS;
    // Карточка покупки рисуется ДО замка раздела: её предпросмотр и
    // «Тестовая покупка» должны работать и до того, как завесу закрыли, -
    // вид оверлея настраивают заранее.
    g.card(ui, p, t("Purchase card"), "карточка уведомление покупка положение корпус рамка радиус toast card notify position chassis border radius", |w| {
        // Предпоказ живёт не только пока тянут ползунок: у корпуса и галочек
        // «тянут» не бывает вовсе, а посмотреть на результат надо (жалоба
        // 2026-08-23). Поэтому любая правка в этой плитке продлевает показ
        // образца на пару секунд.
        let before = x.changes.len();
        check(ui, &mut x.changes, t("Show in game"), "twitch_notify_hud", &mut x.c.twitch_notify_hud);
        // Остальное - под кнопкой: полтора десятка ползунков растягивали свой
        // столбец на весь экран. Кнопка, а НЕ `CollapsingHeader`: тот тянется
        // на ширину ОКНА, а не плитки (правило из шапки файла).
        let open = TOAST_OPEN.load(Ordering::Relaxed);
        let caption = format!("{} {}", t("Look and position"), if open { "-" } else { "+" });
        if button(ui, caption) {
            TOAST_OPEN.store(!open, Ordering::Relaxed);
        }
        if !open {
            x.preview_toast = TOAST_PREVIEW_UNTIL
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|until| until > std::time::Instant::now());
            return;
        }
        slider(ui, &mut x.changes, t("Seconds on screen"), "twitch_notify_secs", &mut x.c.twitch_notify_secs, 1.0, 30.0);
        let mut live = ui.is_item_active();
        // Обычные слайдеры, а не `font_slider`: кегли карточки живут мимо
        // `ui_scale` - общий масштаб панели не должен двигать её размеры.
        slider(ui, &mut x.changes, t("Labels"), "toast_label_size", &mut x.c.toast_label_size, 8.0, 40.0);
        live |= ui.is_item_active();
        slider(ui, &mut x.changes, t("Title"), "toast_value_size", &mut x.c.toast_value_size, 8.0, 48.0);
        live |= ui.is_item_active();
        slider(ui, &mut x.changes, t("Max width"), "toast_width", &mut x.c.toast_width, 120.0, 600.0);
        let secs_active = live | ui.is_item_active();
        // Корпус у карточки свой, не общий с панелью: панель висит постоянно
        // и не должна мешать, а карточка появляется на секунды и обязана
        // читаться сразу (запрос 2026-08-23).
        group(ui, x.c, t("FRAME"));
        chassis_picker(ui, CHASSIS_TOAST, x.c, &mut x.changes);
        if x.c.toast_style != crate::config::PanelStyle::Bare {
            slider(ui, &mut x.changes, t("Background"), "toast_opacity", &mut x.c.toast_opacity, 0.0, 1.0);
        }
        ui.dummy([1.0, 3.0 * x.k]);
        ui.dummy([1.0, 3.0 * x.k]);
        // Та же ручка, что у панели статистики, и тот же слот перетаскивания:
        // одновременно активной может быть только одна - разделы разные.
        let accent = x.c.accent_color;
        let (dragging, released) = crate::overlay::xy_pad(ui, "##toast_xy", &mut x.c.toast_x, &mut x.c.toast_y, accent, x.drag, w);
        if released {
            x.changes.push(("toast_x", x.c.toast_x.to_string()));
            x.changes.push(("toast_y", x.c.toast_y.to_string()));
        }
        hint(ui, t("Drag the dot. Shift - one axis only."));
        slider(ui, &mut x.changes, t("Horizontal"), "toast_x", &mut x.c.toast_x, 0.0, 3840.0);
        let x_active = ui.is_item_active();
        slider(ui, &mut x.changes, t("Vertical"), "toast_y", &mut x.c.toast_y, 0.0, 2160.0);
        // Пока двигают - показываем настоящую карточку на её месте: иначе
        // положение выставляется вслепую, по одной точке в рамке.
        let live_now = secs_active || dragging || x_active || ui.is_item_active();
        if live_now || x.changes.len() != before {
            *TOAST_PREVIEW_UNTIL.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::time::Instant::now() + TOAST_PREVIEW_LINGER);
        }
        x.preview_toast = TOAST_PREVIEW_UNTIL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|until| until > std::time::Instant::now());
        ui.dummy([1.0, 3.0 * x.k]);
        if button(ui, t("Test purchase")) {
            x.test_purchase = true;
        }
    });

    // Награды без подключения настраивать бессмысленно: их не с чем
    // сопоставлять и некому покупать. Замок - не пустой экран, а тот же
    // список, только погашенный: видно, что здесь будет, когда подключишься.
    //
    // `begin_disabled` глушит и мышь, и переход по Tab - именно этого просили.
    // Плитки рисует наш draw list, до которого стиль ImGui не достаёт,
    // поэтому им прозрачность выставляется отдельно.
    //
    // В поиске не запираем: плитки наград приходят вперемешку с чужими, а
    // кнопки «ОК» над ними нет - завеса рисуется только на своём разделе.
    let locked = !x.c.rewards_notice_seen && g.query.is_empty();
    let _off = ui.begin_disabled(locked);
    // Прозрачность возвращаем в конце: в поиске после наград рисуются плитки
    // других разделов, и они гаснуть не должны.
    let alpha = g.alpha;
    if locked {
        g.alpha *= LOCKED_ALPHA;
    }

    // Плитка «Очередь покупок» удалена по запросу 2026-09-08. Сама очередь
    // осталась: на ней держится разнос покупок во времени и правило «одна
    // заявка спавна за раз». Настраивать в ней было нечего - длину теперь
    // задаёт константа `actions::QUEUE_LIMIT`.
    g.card(ui, p, t("On Twitch"), "твич награды включить выключить twitch rewards on off", |_| {
        if button(ui, t("Turn all off")) {
            x.enable_all_rewards = Some(false);
        }
        ui.same_line_with_spacing(0.0, 8.0);
        if button(ui, t("Turn all on")) {
            x.enable_all_rewards = Some(true);
        }
        hint(ui, t("Connecting to Twitch turns the mod's own rewards back on."));
        ui.dummy([1.0, 3.0 * x.k]);
        hint(ui, t("Points are refunded when: the reward is on cooldown, the game is in a menu, a cutscene or a loading screen, the game window is not focused, or the action never happened."));
    });

    // Живёт в «Наградах», а не в «Панели»: настраивают его вместе со спавном,
    // ради которого он и существует (запрос 2026-09-08).
    g.card(ui, p, t("Allies"), "союзник союзники призыв здоровье ally allies summon health", |w| {
        check(ui, &mut x.changes, t("Show allies"), "show_allies", &mut x.c.show_allies);
        if !x.c.show_allies {
            return;
        }
        let before = x.changes.len();
        let accent = x.c.accent_color;
        let (dragging, released) =
            crate::overlay::xy_pad(ui, "##ally_xy", &mut x.c.ally_x, &mut x.c.ally_y, accent, x.drag, w);
        if released {
            x.changes.push(("ally_x", x.c.ally_x.to_string()));
            x.changes.push(("ally_y", x.c.ally_y.to_string()));
        }
        hint(ui, t("Drag the dot. Shift - one axis only."));
        slider(ui, &mut x.changes, t("Horizontal"), "ally_x", &mut x.c.ally_x, 0.0, 3840.0);
        let x_active = ui.is_item_active();
        slider(ui, &mut x.changes, t("Vertical"), "ally_y", &mut x.c.ally_y, 0.0, 2160.0);
        // Пока двигают - рисуем образец на его месте: союзников в мире может не
        // быть вовсе, а положение выставляют заранее. Штамп времени, а не флаг:
        // у галочки «тянут» не бывает, и образец мелькнул бы на один кадр - тот
        // же приём, что у карточки покупки.
        if dragging || x_active || ui.is_item_active() || x.changes.len() != before {
            *ALLY_PREVIEW_UNTIL.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::time::Instant::now() + TOAST_PREVIEW_LINGER);
        }
        x.preview_ally = ALLY_PREVIEW_UNTIL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|until| until > std::time::Instant::now());
    });

    g.card(ui, p, t("Enemy spawns"), "спавн враг лимит радиус жизнь spawn enemy limit radius ttl", |_| {
        // Общий потолок нужен только отладочному спавну: пепел и так не пускает
        // в мир больше своих пяти, и ползунок до полусотни там ничего не значит.
        if x.c.debug_spawn {
            int_slider(ui, &mut x.changes, t("Max summoned"), "spawn_limit", &mut x.c.spawn_limit, 1, 50);
        }
        slider(ui, &mut x.changes, t("Distance, m"), "spawn_radius_m", &mut x.c.spawn_radius_m, 2.0, 60.0);
        check(ui, &mut x.changes, t("Ahead of the camera"), "spawn_in_front", &mut x.c.spawn_in_front);
        check(ui, &mut x.changes, t("Spawn enemies during a boss fight"), "spawn_in_boss", &mut x.c.spawn_in_boss);
        check(ui, &mut x.changes, t("Spawn allies during a boss fight"), "ally_spawn_in_boss", &mut x.c.ally_spawn_in_boss);
        hint(ui, t("Which enemy - picked per reward."));
        ui.dummy([1.0, 3.0 * x.k]);
        // Единственный признак, что спавн вообще происходит: лога в моде нет,
        // а врага за спиной можно и не заметить.
        ui.text_disabled(format!("{}: {}", t("alive right now"), x.spawned));
        if x.spawned > 0 {
            ui.same_line_with_spacing(0.0, 12.0);
            if button(ui, t("Remove all")) {
                x.clear_spawns = true;
            }
        }
        // Кто уронил игру в прошлый запуск (см. «Кто уронил игру» в
        // `spawn.rs`). Молча исключать из случайных нельзя - это выглядело бы
        // как «награда иногда не даёт этого врага».
        if button(ui, t("Check the list")) {
            x.audit_spawns = true;
        }
        // ponytail: временная диагностика, удалить вместе с остальной. Без
        // `t()` намеренно: это отладочная строка, а не подпись интерфейса.
        if let Some(raw) = crate::spawn::last_candle() {
            hint(ui, &format!("candle: {raw}"));
        }
        if crate::spawn::rays_off() {
            hint(ui, t("placement rays crashed the game last run - off for this session"));
        }
        let blamed = crate::spawn::blamed();
        if !blamed.is_empty() {
            let names: Vec<String> = blamed.iter().map(|k| crate::spawn::label(k)).collect();
            hint(ui, &format!("{}: {}", t("crashed the game last run - out of Random"), names.join(", ")));
            if button(ui, t("Forgive")) {
                crate::spawn::forgive();
            }
        }
    });

    g.card(ui, p, t("Effects"), "эффект статус скорость выносливость фляга effect status speed stamina flask", |_| {
        hint(ui, t("Which effect - picked per reward."));
        ui.dummy([1.0, 3.0 * x.k]);
        // Единственный признак, что эффект вообще висит: лога в моде нет, а
        // «почему всё медленно» иначе не объяснить.
        ui.text_disabled(format!("{}: {}", t("running right now"), x.effects_on));
        if x.effects_on > 0 {
            ui.same_line_with_spacing(0.0, 12.0);
            if button(ui, t("Clear effects")) {
                x.clear_effects = true;
            }
        }
    });

    g.wide(ui, p, t("Rewards"), "награда клавиша спавн эффект создать reward key effect create", |w| {
        reward_list(ui, x, w);
    });
    g.alpha = alpha;
}

/// Редактор списка наград.
///
/// Пишет в свой файл, а не в `.ini`, поэтому идёт мимо `changes`: там пары
/// «ключ - строка» под ini, здесь целые записи.
///
/// Раскладка переделана 2026-08-23 («не нравится, как выглядит добавление
/// награды и плашки»). Что изменилось по существу, а не по красоте:
///
/// 1. **Аккордеон.** Развёрнута всегда одна награда. Десяток раскрытых разом
///    превращал список в простыню, по которой не найти нужную.
/// 2. **Своя шапка вместо `CollapsingHeader`.** Тот тянется на всю ширину
///    ОКНА, а не плитки (правило из шапки файла), и его нельзя открыть из
///    кода - а «Новая награда» обязана сразу открыться и поймать курсор.
/// 3. **Статус на Twitch стал тремя состояниями**, а не одним значком:
///    «не создана» / «на Twitch» / «изменено». Третьего не было вовсе, и
///    правка цены молча не доезжала до дашборда.
/// 4. **Подписи над полями, а не сбоку.** Сбоку они делили ширину с самим
///    полем и обрезались (`labeled`), а места в этой плитке полно - она
///    единственная во всю ширину раздела.
/// 5. **Удаление отодвинуто к правому краю и требует двух кликов**: раньше
///    оно стояло вплотную к «Создать на Twitch».
/// 6. **Карточка в две строки** - название и цена сверху, сводка действия
///    под ними. Полосой в одну строку список читался журналом, а не набором
///    наград.
/// 7. **Кликается карточка целиком** - `invisible_button` во всю её площадь.
///    Галочка «включена» рисуется ДО подложки: в ImGui hover достаётся тому,
///    кто попросил его раньше в кадре (`ItemHoverable` отдаёт `false`, если
///    `HoveredId` уже занят), и в обратном порядке подложка отобрала бы у
///    галочки и наведение, и клик. Всё, что после подложки, - текст и наш
///    draw list: они hover не перехватывают вовсе.
/// 8. **Стоят сеткой, а не бутербродом**: тайл по содержимому шириной с
///    названием, действием, ценой, меткой Twitch и крестиком. Курсор по
///    сетке ведётся руками, поэтому в конце нужен `dummy`: ImGui иначе
///    посчитал бы высоту плитки по последнему тайлу.
/// 9. **Редактор - отдельное плавающее окно, а не раскрытие в сетке**
///    (запрос 2026-08-24: раньше выбранная плитка росла на своём месте и
///    толкала соседей). Сама сетка теперь рисует только тайлы; полную
///    карточку с полями и действием смотри в `reward_editor_window` -
///    выбор там переставляет `OPEN_REWARD` и не открывает второе окно.
fn reward_list(ui: &Ui, x: &mut Ctx, w: f32) {
    let (accent, k) = (x.c.accent_color, x.k);
    let alpha = ui.clone_style().alpha;
    let dt = ui.io().delta_time.clamp(0.0, 0.1);

    // Главное действие плитки - акцентом: остальные кнопки здесь тусклые, и
    // с ними в ряд она читалась как ещё одна второстепенная.
    let _hot = ui.push_style_color(StyleColor::Button, crate::overlay::with_alpha(accent, 0.34));
    let add = button(ui, t("+ New reward"));
    drop(_hot);
    if add {
        let id = rewards::next_id(x.rewards);
        x.rewards.push(RewardEntry {
            id,
            enabled: true,
            action: Action::Press { key: hudhook::imgui::Key::Space },
            cost: 100,
            reward_id: String::new(),
            reward_title: String::new(),
            label: String::new(),
            synced: 0,
        });
        x.rewards_dirty = true;
        // Сразу открыть и поставить курсор в название: раньше появлялась
        // плашка «Без названия», и куда печатать - непонятно.
        *OPEN_REWARD.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
        *FOCUS_TITLE.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
    }
    if !x.rewards.is_empty() {
        ui.same_line_with_spacing(0.0, 12.0 * k);
        ui.text_disabled(format!("{}: {}", t("total"), x.rewards.len()));
    }
    if let Some(text) = x.notice {
        ui.text_colored(accent, text);
    }
    if x.rewards.is_empty() {
        hint(ui, t("A reward links a Twitch button to an action in the game. Add one here, pick the action, then press Create on Twitch."));
        return;
    }
    ui.dummy([1.0, 4.0 * k]);

    let open = *OPEN_REWARD.lock().unwrap_or_else(|e| e.into_inner());
    let mut remove = None;
    let mut toggled = None;

    // Свёрнутые награды стоят сеткой, раскрытая - во всю ширину на своём месте
    // в этой же сетке (запрос 2026-08-23: «рядом, а не бутербродом»). Курсор
    // ведём руками: ImGui кладёт элементы построчно и о сетке не знает.
    let gap = 8.0 * k;
    // Ширина у каждого тайла своя, по содержимому: у награды «Прыжок» и у
    // «Спавн Гнилой древо-аватар» в сетке одинаковой ширины половина плитки
    // пустовала (запрос 2026-08-23). Ряды набираются потоком и переносятся,
    // когда очередной тайл не влезает.
    let min_tile_w = 150.0 * k;
    let line = ui.text_line_height();
    // Две строки: название с галочкой и цена со статусом. Описание действия
    // из тайла убрано (запрос 2026-08-23) - оно есть в раскрытой карточке, а
    // здесь занимало ряд и ничего не решало.
    let tile_h = line * 2.0 + 20.0 * k;
    let origin = ui.cursor_pos();
    let origin_s = ui.cursor_screen_pos();
    // Сколько занято в текущем ряду. Ноль - ряд пуст.
    let mut cx = 0.0f32;
    let mut y = 0.0f32;

    for index in 0..x.rewards.len() {
        let id = x.rewards[index].id;
        let _row = ui.push_id_usize(index);
        // Отмечает, чья карточка сейчас в отдельном окне редактора -
        // `reward_editor_window` рисуется независимо, здесь только подсветка
        // тайла (запрос 2026-08-24: «вместо разворачивания - открывать
        // отдельное окно»).
        let opened = open == Some(id);
        let dying = *DYING.lock().unwrap_or_else(|e| e.into_inner()) == Some(id);

        // Раскрытия у тайла больше нет - осталось только появление/исчезновение.
        let mut a = row_metrics(id);
        a.life = approach(a.life, if dying { 0.0 } else { 1.0 }, dt, 0.10);
        // Погасла до конца - вот теперь её можно убрать из списка.
        if dying && a.life < 0.02 {
            remove = Some(index);
        }
        let live = a.life.clamp(0.0, 1.0);
        // Гасим и виджеты ImGui (текст, кнопки), и свою отрисовку: стиль
        // ImGui до нашего draw list не достаёт.
        let card_alpha = alpha * live;
        let _fade = (live < 0.999)
            .then(|| ui.push_style_var(StyleVar::Alpha(ui.clone_style().alpha * live)));

        // Ширина тайла - по его содержимому. Считается до расстановки: от неё
        // зависит, влезет ли он в текущий ряд.
        let badge = match x.rewards[index].in_sync() {
            None => t("not created"),
            Some(true) => t("on Twitch"),
            Some(false) => t("changed"),
        };
        let title = if x.rewards[index].reward_title.trim().is_empty() {
            t("Untitled").to_string()
        } else {
            x.rewards[index].reward_title.trim().to_string()
        };
        let cost_text = x.rewards[index].cost.to_string();
        let cross = 20.0 * k;
        let check_w = ui.frame_height();
        let head_w = 8.0 * k + check_w + 6.0 * k + ui.calc_text_size(&title)[0]
            + 8.0 * k + cross * 2.0 + 10.0 * k + 9.0 * k;
        let foot_w = 8.0 * k + check_w + 6.0 * k + ui.calc_text_size(&cost_text)[0]
            + 12.0 * k + ui.calc_text_size(badge)[0] + 9.0 * k;
        let tile_w = head_w.max(foot_w).clamp(min_tile_w, w);

        // Тайл переносится, когда не влезает в остаток строки.
        if cx > 0.0 && cx + tile_w > w {
            cx = 0.0;
            y += tile_h + gap;
        }
        let pos = [origin[0] + cx, origin[1] + y];
        let pos_s = [origin_s[0] + cx, origin_s[1] + y];
        ui.set_cursor_pos(pos);

        {
            let min = pos_s;
            let max = [pos_s[0] + tile_w, pos_s[1] + tile_h];
            let over = ui.is_mouse_hovering_rect(min, max);
            a.hov = approach(a.hov, if over { 1.0 } else { 0.0 }, dt, 0.09);
            let hov = a.hov;
            // Взведённое удаление красит карточку целиком: два одинаковых
            // клика подряд не читались как «нажми ещё раз, чтобы удалить»
            // (жалоба 2026-08-23).
            let armed = DELETE_ARMED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|(b, at)| b == id && at.elapsed().as_secs_f32() < RESET_ARMED_SECS);
            // Открытая в окне редактора - рамка ярче не только на ховере:
            // единственный признак на плитке, какая из них сейчас правится.
            let sel = if opened { 1.0 } else { 0.0 };
            let (fill, border) = match armed {
                true => (
                    [0.30, 0.08, 0.06, (0.70 + 0.10 * hov) * card_alpha],
                    [0.85, 0.30, 0.24, (0.75 + 0.20 * hov) * card_alpha],
                ),
                false => (
                    [0.10, 0.09, 0.07, (0.34 + 0.14 * hov + 0.08 * sel) * card_alpha],
                    crate::overlay::with_alpha(accent, (0.14 + 0.24 * hov + 0.30 * sel) * card_alpha),
                ),
            };
            crate::overlay::card_frame(min, max, fill, border, 7.0 * k);
            remember_row(id, a);

            // Кнопки - ПЕРВЫМИ, до подложки: в ImGui hover достаётся тому, кто
            // попросил его раньше в кадре, и в обратном порядке подложка
            // отобрала бы у них и наведение, и клик.
            let cross = 20.0 * k;
            let btn_y = pos[1] + 6.0 * k;
            let _flat = ui.push_style_var(StyleVar::FrameBorderSize(0.0));
            let _tight = ui.push_style_var(StyleVar::FramePadding([0.0, 0.0]));

            // «Проверить» - тот же размер, что у крестика: обе живут в углу
            // карточки и обе про одну награду.
            // Включатель - здесь же, до подложки: иначе она заберёт и клик,
            // и наведение (правило приоритета hover в ImGui).
            ui.set_cursor_pos([pos[0] + 8.0 * k, btn_y]);
            let mut on = x.rewards[index].enabled;
            if ui.checkbox("###tile_en", &mut on) {
                x.rewards[index].enabled = on;
                x.rewards_dirty = true;
            }
            let check_w = ui.item_rect_size()[0];

            ui.set_cursor_pos([pos[0] + tile_w - cross * 2.0 - 10.0 * k, btn_y]);
            let play = [
                ui.push_style_color(StyleColor::Button, [0.0, 0.0, 0.0, 0.0]),
                ui.push_style_color(StyleColor::ButtonHovered, crate::overlay::with_alpha(accent, 0.30)),
                ui.push_style_color(StyleColor::ButtonActive, crate::overlay::with_alpha(accent, 0.45)),
            ];
            let play_at = ui.cursor_screen_pos();
            if ui.button_with_size("###try", [cross, cross]) {
                x.test_reward = Some(id);
            }
            for c in play {
                c.end();
            }
            // Треугольник рисуем сами: в атласе только Latin-1 и кириллица,
            // любой значок «play» вышел бы знаком вопроса.
            crate::overlay::caret(
                [play_at[0] + cross * 0.5, play_at[1] + cross * 0.5],
                9.0 * k,
                false,
                crate::overlay::with_alpha(accent, (0.55 + 0.35 * hov) * card_alpha),
            );

            ui.set_cursor_pos([pos[0] + tile_w - cross - 6.0 * k, btn_y]);
            let paint = [
                ui.push_style_color(
                    StyleColor::Button,
                    if armed { [0.75, 0.24, 0.20, 0.95] } else { [0.0, 0.0, 0.0, 0.0] },
                ),
                ui.push_style_color(StyleColor::ButtonHovered, [0.55, 0.18, 0.14, 0.75]),
                ui.push_style_color(StyleColor::ButtonActive, [0.65, 0.22, 0.18, 0.90]),
            ];
            if ui.button_with_size("×###del", [cross, cross]) {
                let mut slot = DELETE_ARMED.lock().unwrap_or_else(|e| e.into_inner());
                match armed {
                    // Второй клик не удаляет сразу: карточка сперва гаснет, и
                    // из списка её убирает `life` на нуле.
                    true => {
                        *DYING.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
                        *slot = None;
                    }
                    false => *slot = Some((id, std::time::Instant::now())),
                }
            }
            for c in paint {
                c.end();
            }
            drop(_tight);
            drop(_flat);

            // Подложка на всю карточку: раскрывается кликом куда угодно, кроме
            // крестика. Всё, что после неё, - текст и наш draw list, а они
            // hover не перехватывают.
            ui.set_cursor_pos(pos);
            if ui.invisible_button("###open", [tile_w, tile_h]) {
                toggled = Some(id);
            }

            let badge_color = match x.rewards[index].in_sync() {
                None => ui.style_color(StyleColor::TextDisabled),
                Some(true) => accent,
                Some(false) => CHANGED,
            };
            let tx0 = pos[0] + 8.0 * k + check_w + 6.0 * k;
            let room = pos[0] + tile_w - 9.0 * k - tx0;
            ui.set_cursor_pos([tx0, pos[1] + 7.0 * k]);
            // Выключенная награда - приглушённым: видно, что она в списке
            // есть, а в игре не сработает.
            let head = clip_to(ui, &title, room - cross * 2.0 - 6.0 * k);
            match x.rewards[index].enabled {
                true => ui.text(head),
                false => ui.text_disabled(head),
            }

            let cost_w = ui.calc_text_size(&cost_text)[0];
            let cost_y = pos[1] + 7.0 * k + line + 3.0 * k;
            ui.set_cursor_pos([tx0, cost_y]);
            ui.text_colored(crate::overlay::with_alpha(accent, 0.85 * alpha), &cost_text);
            // Взведённое удаление говорит об этом прямо на месте статуса.
            let (tail, tail_color) = match armed {
                true => (t("Click again to delete"), [0.95, 0.55, 0.50, 1.0]),
                false => (badge, badge_color),
            };
            let tail = clip_to(ui, tail, room - cost_w - 12.0 * k);
            let tail_w = ui.calc_text_size(&tail)[0];
            ui.set_cursor_pos([pos[0] + tile_w - 9.0 * k - tail_w, cost_y]);
            ui.text_colored(tail_color, tail);

            cx += tile_w + gap;
        }
    }

    // Сетку ImGui не видит: элементы расставлены курсором вручную, и без этого
    // плитка посчитала бы свою высоту по последнему тайлу, а не по всей сетке.
    if cx > 0.0 {
        y += tile_h + gap;
    }
    ui.set_cursor_pos([origin[0], origin[1] + y]);
    ui.dummy([1.0, 1.0]);

    if let Some(id) = toggled {
        let mut open = OPEN_REWARD.lock().unwrap_or_else(|e| e.into_inner());
        // Та же награда - закрыть окно редактора. Другая - переключить его на
        // неё: то же окно, не второе (запрос 2026-08-24).
        *open = if *open == Some(id) { None } else { Some(id) };
        // Взведённое удаление чужой награды сбрасываем: оно относилось к той,
        // которую только что закрыли или с которой переключились.
        *DELETE_ARMED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    if let Some(index) = remove {
        // Убрать надо и с дашборда, иначе награда там остаётся живой: зритель
        // покупает то, чего в моде больше нет (жалоба 2026-08-23).
        let gone = x.rewards.remove(index);
        if !gone.reward_id.is_empty() {
            x.delete_on_twitch = Some(gone.reward_id);
        }
        // Ждать нажатия для удалённой награды больше некому.
        *x.capture = None;
        x.rewards_dirty = true;
        *DYING.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // Раскрытой её больше нет - иначе следующая награда с этим номером
        // (а номера не переиспользуются) открылась бы сама.
        let mut open = OPEN_REWARD.lock().unwrap_or_else(|e| e.into_inner());
        if *open == Some(gone.id) {
            *open = None;
        }
    }
}

/// Плавающее окно редактора одной награды.
///
/// Раньше клик по плитке раскрывал её прямо в сетке (см. историю в шапке
/// файла): выбранная росла на своём месте и толкала соседей, а переключение
/// между наградами ждало анимацию сворачивания-разворачивания. По запросу
/// 2026-08-24 редактор переехал в обычное плавающее окно ImGui - вне сетки
/// клипинг и позиционирование ей больше не нужны вовсе, `reward_action` и так
/// был написан на относительной раскладке (`set_next_item_width`,
/// `same_line_with_spacing`) и переехал без единой правки.
///
/// **Окно одно, с постоянным id** (`###reward_editor` - `###` прибивает id
/// отдельно от заголовка, иначе смена названия награды завела бы ImGui новым
/// окном при каждой правке текста, см. историю про вкладки и `###`). Клик по
/// другой плитке просто переставляет `OPEN_REWARD` (см. `reward_list`), и на
/// следующем кадре это же окно перерисовывается с содержимым другой награды -
/// ровно то, что просили: «выбор другой карточки обновляет открытое окно, а
/// не создаёт новое».
fn reward_editor_window(ui: &Ui, x: &mut Ctx) {
    let open_id = *OPEN_REWARD.lock().unwrap_or_else(|e| e.into_inner());
    if open_id.is_some() {
        *REWARD_EDITOR_LAST.lock().unwrap_or_else(|e| e.into_inner()) = open_id;
    }

    // Появление и уход тянутся во времени, как окно настроек само: вниз
    // быстрее (0.12с), чем вверх (0.15с).
    const FADE_IN: f32 = 0.15;
    const FADE_OUT: f32 = 0.12;
    let dt = ui.io().delta_time.clamp(0.0, 0.1);
    let target = if open_id.is_some() { 1.0 } else { 0.0 };
    let fade = {
        let mut slot = REWARD_EDITOR_FADE.lock().unwrap_or_else(|e| e.into_inner());
        *slot = approach(*slot, target, dt, if target > *slot { FADE_IN } else { FADE_OUT });
        *slot
    };
    // Совсем погасло - рисовать нечего, а `REWARD_EDITOR_LAST` можно не
    // трогать: следующее открытие перезапишет его само.
    if fade < 0.01 {
        return;
    }
    let Some(id) = *REWARD_EDITOR_LAST.lock().unwrap_or_else(|e| e.into_inner()) else {
        return;
    };
    let Some(index) = x.rewards.iter().position(|e| e.id == id) else {
        // Награда пропала (удалена не отсюда) - закрывать нечего.
        *OPEN_REWARD.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return;
    };

    let k = x.k;
    let accent = x.c.accent_color;
    let title = if x.rewards[index].reward_title.trim().is_empty() {
        t("Untitled").to_string()
    } else {
        x.rewards[index].reward_title.trim().to_string()
    };

    // Заголовок несёт название награды, а id окна - отдельно от него.
    let mut window_open = true;
    // Удаление закрывает окно в тот же клик, что гасит плитку.
    let mut close_now = false;
    // Пока не полностью открыто - мышь мимо: клик «сквозь» ещё прозрачное
    // или уже гаснущее окно попал бы по виджету, которого не должно быть
    // видно (тот же приём, что у самого окна настроек).
    let flags = if fade < 0.999 { WindowFlags::NO_MOUSE_INPUTS } else { WindowFlags::empty() };
    let _fade = ui.push_style_var(StyleVar::Alpha(ui.clone_style().alpha * fade.clamp(0.0, 1.0)));
    ui.window(format!("{title}###reward_editor"))
        .flags(flags)
        .opened(&mut window_open)
        .size([460.0 * k, 520.0 * k], Condition::FirstUseEver)
        .build(|| {
            let inner = ui.content_region_avail()[0];

            let mut enabled = x.rewards[index].enabled;
            if ui.checkbox(t("Enabled"), &mut enabled) {
                x.rewards[index].enabled = enabled;
                x.rewards_dirty = true;
            }
            let (badge, badge_color) = match x.rewards[index].in_sync() {
                None => (t("not created"), ui.style_color(StyleColor::TextDisabled)),
                Some(true) => (t("on Twitch"), accent),
                Some(false) => (t("changed"), CHANGED),
            };
            let badge_w = ui.calc_text_size(badge)[0] + PILL_PAD_X * 2.0 * k;
            ui.same_line_with_pos((inner - badge_w).max(0.0));
            status_pill(ui, badge, badge_color, k);

            // Сводка одной строкой («нажать Пробел», «спавн Тролль») - тайл
            // в сетке её не показывает, а открывать раздел «В ИГРЕ» ради
            // одного взгляда не нужно.
            ui.text_disabled(clip_to(ui, &describe(&x.rewards[index].action), inner));

            // Разделитель между «что это» и «что с этим делать» - две разные
            // мысли читаются раздельно, а не одним потоком текста и кнопок.
            ui.dummy([1.0, 6.0 * k]);
            crate::overlay::rule_line(ui.cursor_screen_pos(), inner, crate::overlay::with_alpha(accent, 0.14));
            ui.dummy([1.0, 8.0 * k]);

            // Кнопки наверху, до полей (то же правило, что было в сетке): за
            // ними и открывают награду, а листать до низа ради «Проверить»
            // неудобно.
            if button(ui, t("Try it")) {
                x.test_reward = Some(id);
            }
            ui.same_line_with_spacing(0.0, 10.0 * k);
            let published = !x.rewards[index].reward_id.is_empty();
            let caption = if published {
                t("Update on Twitch")
            } else {
                t("Create on Twitch")
            };
            if x.connected && !x.rewards[index].reward_title.trim().is_empty() {
                let hot = x.rewards[index].in_sync() != Some(true);
                let _lit = hot.then(|| {
                    ui.push_style_color(StyleColor::Button, crate::overlay::with_alpha(accent, 0.40))
                });
                if button(ui, caption) {
                    x.create_reward = Some(id);
                }
            } else {
                ui.text_disabled(caption);
            }

            // Удаление у правого края и в два клика, как раньше - промах
            // мышью иначе стирал бы награду вместе с её id на Twitch.
            let armed = DELETE_ARMED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|(b, at)| b == id && at.elapsed().as_secs_f32() < RESET_ARMED_SECS);
            let del = if armed { t("Click again to delete") } else { t("Delete") };
            let del_w = ui.calc_text_size(del)[0] + 18.0 * k;
            ui.same_line_with_pos((inner - del_w).max(0.0));
            let _warn = armed.then(|| ui.push_style_color(StyleColor::Button, [0.75, 0.24, 0.20, 0.95]));
            if button(ui, del) {
                let mut slot = DELETE_ARMED.lock().unwrap_or_else(|e| e.into_inner());
                match armed {
                    // Второй клик не удаляет сразу: плитка сперва гаснет
                    // (`DYING`) - окно редактора закрывается с ней вместе.
                    true => {
                        *DYING.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
                        *slot = None;
                        close_now = true;
                    }
                    false => *slot = Some((id, std::time::Instant::now())),
                }
            }
            drop(_warn);

            // Заголовок секции: подпись акцентом и линейка под ней - тот же
            // язык, что у заголовков плиток в остальном окне.
            let section = |ui: &Ui, name: &str| {
                ui.dummy([1.0, 8.0 * k]);
                ui.text_colored(crate::overlay::with_alpha(accent, 0.70), name);
                ui.dummy([1.0, 1.0 * k]);
                crate::overlay::rule_line(
                    ui.cursor_screen_pos(),
                    inner,
                    crate::overlay::with_alpha(accent, 0.18),
                );
                ui.dummy([1.0, 5.0 * k]);
            };

            // --- блок 1: чем это является на Twitch ---
            section(ui, t("ON TWITCH"));
            {
                let e = &mut x.rewards[index];
                ui.text_disabled(t("Title on Twitch"));
                ui.set_next_item_width(inner);
                if *FOCUS_TITLE.lock().unwrap_or_else(|e| e.into_inner()) == Some(id) {
                    // Действует на СЛЕДУЮЩИЙ виджет, поэтому стоит вплотную.
                    ui.set_keyboard_focus_here();
                    *FOCUS_TITLE.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
                // Пишем по отпусканию: `build()` возвращает true на КАЖДУЮ
                // набранную букву, и файл наград переписывался бы столько же
                // раз за одно название.
                let _ = ui
                    .input_text("###rtitle", &mut e.reward_title)
                    .hint(t("exactly as on the Twitch dashboard"))
                    .build();
                x.rewards_dirty |= ui.is_item_deactivated_after_edit();

                let half = (inner - 12.0 * k) * 0.5;
                ui.text_disabled(t("Cost, points"));
                ui.same_line_with_pos(half + 12.0 * k);
                ui.text_disabled(t("Caption in the HUD"));

                let mut cost = e.cost as i32;
                ui.set_next_item_width(half);
                if ui.input_int("###rcost", &mut cost).build() {
                    e.cost = cost.clamp(1, 1_000_000) as u32;
                }
                x.rewards_dirty |= ui.is_item_deactivated_after_edit();
                ui.same_line_with_pos(half + 12.0 * k);
                ui.set_next_item_width(half);
                let _ = ui
                    .input_text("###rlabel", &mut e.label)
                    .hint(t("optional - the title is used"))
                    .build();
                x.rewards_dirty |= ui.is_item_deactivated_after_edit();
            }

            // --- блок 2: что происходит в игре ---
            section(ui, t("IN GAME"));
            reward_action(ui, x, index, inner);

            if !published {
                // Возврат баллов Twitch разрешает только тому приложению,
                // которое награду создало. Заведённую руками на дашборде мод
                // отменить не может ничем.
                hint(ui, t("Created outside the mod - Twitch will not refund points for it."));
            }
            if !x.connected {
                hint(ui, t("Creating and updating rewards needs a connected Twitch."));
            } else if x.rewards[index].in_sync() == Some(false) {
                hint(ui, t("The title or cost on Twitch differs - press Update on Twitch."));
            }
        });

    if !window_open || close_now {
        *OPEN_REWARD.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *DELETE_ARMED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Что награда делает в игре: тип действия и его параметры.
///
/// Отдельной функцией, а не куском `reward_list`: там уже три блока, и этот -
/// единственный, который сам по себе длиннее остальных вместе взятых.
fn reward_action(ui: &Ui, x: &mut Ctx, index: usize, inner: f32) {
    let (accent, k) = (x.c.accent_color, x.k);
    let id = x.rewards[index].id;
    let current = x.rewards[index].action;

    // «Держать» - больше не свой пункт выбора, а поле внутри «Нажать» (прямой
    // запрос 2026-08-24): по сути это одно и то же действие, разница только в
    // том, ноль секунд удержания или нет.
    let mut kind = match current {
        Action::Press { .. } | Action::Hold { .. } => Kind::Press,
        Action::SpawnEnemy { .. } => Kind::Spawn,
        Action::Effect { .. } => Kind::Effect,
    };
    let key = current.key();
    // `0` - обычное нажатие. Дефолт тоже `0`, а не старые «полсекунды»: при
    // переключении на «Нажать» с чего угодно ожидается именно тап, а не
    // внезапное удержание.
    let mut hold_ms = match current {
        Action::Hold { duration_ms, .. } => duration_ms,
        _ => 0,
    };
    // `first()`, а не `[0]`: пустая таблица - это паника, а `panic = abort`
    // в релизе превращает её в краш игры.
    let first_spawn = crate::spawn::SPAWN_TABLE.first().map(|e| e.key).unwrap_or("");
    let mut spawn_key: &'static str = match current {
        Action::SpawnEnemy { key, .. } => key,
        _ => first_spawn,
    };
    // Ноль - враг появляется сразу; отсчёт на карточке покупки тогда не
    // рисуется вовсе (запрос 2026-08-25).
    let mut spawn_delay = match current {
        Action::SpawnEnemy { delay_secs, .. } => delay_secs,
        _ => 0,
    };
    let mut spawn_cooldown = match current {
        Action::SpawnEnemy { cooldown_secs, .. } => cooldown_secs,
        _ => 0,
    };
    let mut spawn_ally = match current {
        Action::SpawnEnemy { ally, .. } => ally,
        _ => false,
    };
    // Ноль - срок не выставляли, и умолчание тогда своё у союзника и у врага.
    // Ноль и остаётся в награде, пока ползунок не трогали: иначе галочка
    // «союзник» не меняла бы срок, потому что число уже записано.
    let mut spawn_ttl = match current {
        Action::SpawnEnemy { ttl_secs, .. } => ttl_secs,
        _ => 0,
    };
    let first_effect = crate::effects::EFFECT_TABLE.first().map(|e| e.key).unwrap_or("");
    let mut effect_key: &'static str = match current {
        Action::Effect { key, .. } => key,
        _ => first_effect,
    };
    // `0` значит «срок из таблицы», и именно ноль лежит в свежей награде: так
    // правка дефолта в таблице доезжает до уже настроенных наград.
    let mut effect_secs = match current {
        Action::Effect { secs, .. } => secs,
        _ => 0,
    };
    // Новая награда-эффект по умолчанию работает везде: запрет - это решение
    // стримера, а не наше.
    let mut effect_in_boss = match current {
        Action::Effect { in_boss, .. } => in_boss,
        _ => true,
    };
    // `0` - без перезарядки, и это дефолт для свежей награды (прямой запрос
    // 2026-08-24).
    let mut effect_cooldown = match current {
        Action::Effect { cooldown_secs, .. } => cooldown_secs,
        _ => 0,
    };
    let mut touched = false;
    // `applied` - обновить действие в памяти (каждый кадр перетаскивания),
    // `touched` - записать файл наград (только когда ползунок отпустили).
    let mut applied = false;
    // Как награда звалась бы сама ДО правок этого кадра. Считается один раз из
    // нетронутого действия, а не расставляется по всем местам, где что-то
    // меняют: четыре разных `was = ...` уже дважды разъезжались с тем, что
    // реально поменяли (жалобы 2026-08-23 и 2026-08-24).
    let was = auto_title(&current);

    ui.text_disabled(t("What it does"));
    // Сегменты вместо голых радиокнопок в ряд: активный тип закрашен акцентом
    // вместо точки где-то сбоку - при четырёх вариантах так виднее с первого
    // взгляда, что выбрано сейчас. Каждая смена типа запоминает, как награда
    // называлась ДО неё: только это имя мод вправе стереть или переписать. Не
    // запомнить его значит оставить «Спавн Маления» на награде, которая теперь
    // жмёт клавишу.
    if let Some(new_kind) = kind_picker(ui, accent, k, inner, kind) {
        touched = true;
        // Ждали нажатия клавиши, а тип сменили на спавн или эффект - кнопки
        // «Клавиша» больше нет, и захват висел бы, съев следующее нажатие
        // впустую.
        if matches!(new_kind, Kind::Spawn | Kind::Effect) && *x.capture == Some(id) {
            *x.capture = None;
        }
        kind = new_kind;
    }
    ui.dummy([1.0, 6.0 * k]);

    if kind == Kind::Press {
        // Клавиша назначается нажатием, а не выбором из списка: так не нужно
        // помнить, как в игре называется кнопка переката.
        let waiting = *x.capture == Some(id);
        let caption = if waiting {
            t("Press a key...").to_string()
        } else {
            format!("{}: {}", t("Key"), rewards::key_label(key))
        };
        let _lit = waiting
            .then(|| ui.push_style_color(StyleColor::Button, crate::overlay::with_alpha(accent, 0.45)));
        if button(ui, &caption) {
            *x.capture = if waiting { None } else { Some(id) };
        }
        drop(_lit);
        if waiting {
            ui.same_line();
            ui.text_disabled(t("Esc cancels"));
        }

        // «Держать» слито сюда же: ноль - обычный тап, больше нуля - то, что
        // раньше было отдельным типом «Держать».
        let mut secs = hold_ms as f32 / 1000.0;
        ui.text_disabled(t("Hold for, s (0 - a plain tap)"));
        ui.set_next_item_width(inner * 0.5);
        if ui.slider_config("###rhold", 0.0, MAX_HOLD_MS as f32 / 1000.0).display_format("%.1f").build(&mut secs) {
            hold_ms = (secs * 1000.0) as u32;
            applied = true;
        }
        touched |= ui.is_item_deactivated_after_edit();
    }

    if kind == Kind::Effect {
        // Строк тринадцать, а не двести - поиска и фильтра тут не нужно, всё
        // помещается в один список со скроллом.
        //
        // Высота считается тем же способом, что у списка спавна - до низа окна
        // редактора: фиксированные 280px обрезали список на маленьком экране
        // (жалоба 2026-08-25), а рядом такой же список тянулся во всю высоту.
        let h = (ui.content_region_avail()[1] - LIST_FOOTER * k).max(200.0 * k);
        let list_w = (inner * 0.62).max(200.0 * k);
        let opts_w = (inner - list_w - 10.0 * k).max(110.0 * k);
        // Срок есть только у временных: у разового ползунок настраивал бы ничто.
        let timed = crate::effects::default_secs(effect_key) > 0;

        // `always_use_window_padding` - иначе дочернее окно без рамки ИМЕЕТ
        // нулевой отступ вовсе, невзирая на пуш `WindowPadding` (правило самого
        // ImGui), и содержимое сидит впритык к краю (жалоба 2026-08-24).
        ui.child_window("###effect_opts").size([opts_w, h]).always_use_window_padding(true).build(|| {
            let w = ui.content_region_avail()[0];
            ui.text_disabled(t("What to do"));
            ui.text_colored(accent, clip_to(ui, &crate::effects::label(effect_key), w));
            ui.dummy([1.0, 8.0 * k]);
            // Своя галочка у каждой награды, а не общая настройка: одни эффекты
            // ради боя и покупают, другие посреди боя портят ран.
            if ui.checkbox(t("During a boss fight###reff_boss"), &mut effect_in_boss) {
                applied = true;
                touched = true;
            }
            ui.dummy([1.0, 4.0 * k]);
            // Перезарядка ЭТОЙ награды (прямой запрос 2026-08-24): сколько
            // после покупки её нельзя купить снова - для всех сразу, кто бы ни
            // платил, и только её: соседние награды и другие эффекты идут
            // дальше. Одного зрителя по всем наградам категории ограничивает
            // «Пауза: эффект» в разделе «Очередь покупок» - это разные вещи.
            // Без таймера на экране: покупка в перезарядке возвращает баллы.
            // Годится и разовым эффектам, поэтому стоит ДО развилки по `timed`.
            ui.text_disabled(clip_to(ui, t("Cooldown, s"), w));
            let mut cd = f32::from(effect_cooldown);
            ui.set_next_item_width(w);
            if ui
                .slider_config("###reffcooldown", 0.0, f32::from(crate::effects::MAX_SECS))
                .display_format("%.0f")
                .build(&mut cd)
            {
                effect_cooldown = cd as u16;
                applied = true;
            }
            touched |= ui.is_item_deactivated_after_edit();
            ui.dummy([1.0, 4.0 * k]);
            // Описание идёт ПОД сроком - последней строкой в колонке. Там же,
            // где его читают: сначала настроил, потом свериться, что покупает
            // зритель. У разового срока нет, и оно встаёт под перезарядкой.
            if !timed {
                about(ui, effect_key);
                return;
            }
            ui.text_disabled(clip_to(ui, t("Lasts, s (0 - default)"), w));
            // Значение применяется КАЖДЫЙ кадр, а в файл уходит по отпусканию:
            // иначе ползунок «убегает в ноль» - действие пересоздаётся из
            // списка каждый кадр (жалоба 2026-08-20).
            let mut secs = f32::from(effect_secs);
            ui.set_next_item_width(w);
            if ui
                .slider_config("###reffsecs", 0.0, f32::from(crate::effects::MAX_SECS))
                .display_format("%.0f")
                .build(&mut secs)
            {
                effect_secs = secs as u16;
                applied = true;
            }
            touched |= ui.is_item_deactivated_after_edit();
            about(ui, effect_key);
        });

        ui.same_line_with_spacing(0.0, 10.0 * k);
        ui.child_window("###effect_list").size([list_w, h]).always_use_window_padding(true).build(|| {
            let room = (ui.content_region_avail()[0] - ui.frame_height() - 12.0 * k).max(40.0);
            for e in crate::effects::EFFECT_TABLE {
                let mut chosen = effect_key == e.key;
                let text = clip_to(ui, &crate::effects::label(e.key), room);
                if ui.radio_button(format!("{text}###effect_{}", e.key), &mut chosen, true)
                    && effect_key != e.key
                {
                    effect_key = e.key;
                    // Срок у нового эффекта свой, прежнее число к нему
                    // отношения не имеет: ноль означает «как в списке».
                    effect_secs = 0;
                    touched = true;
                }
            }
        });
    }

    if kind == Kind::Spawn {
        // Параметры слева узкой колонкой, список справа - широкий и высокий,
        // с поиском ПРЯМО НАД ним (запрос 2026-08-23: в прошлой раскладке
        // поиск стоял в чужой колонке и терялся от списка).
        //
        // Высота - до низа окна редактора, а не фиксированные 330px (запрос
        // 2026-08-24: список не рос вместе с окном). Снизу ещё подсказки
        // («не создана», «не подключено», рассинхрон) - их высоту заранее не
        // измерить, текст переносится по ширине, - поэтому запас
        // фиксированный: не хватит - у окна редактора и так есть своя
        // прокрутка, это не катастрофа.
        let h = (ui.content_region_avail()[1] - LIST_FOOTER * k).max(200.0 * k);
        let list_w = (inner * 0.62).max(240.0 * k);
        let opts_w = (inner - list_w - 10.0 * k).max(110.0 * k);
        let mut search = SPAWN_SEARCH.lock().unwrap_or_else(|e| e.into_inner());

        ui.child_window("###spawn_opts").size([opts_w, h]).always_use_window_padding(true).build(|| {
            let w = ui.content_region_avail()[0];
            // Выбранный - отдельной строкой и акцентом: список длинный, и
            // искать в нём отмеченную точку глазами не нужно.
            ui.text_disabled(t("Who to spawn"));
            ui.text_colored(accent, clip_to(ui, &crate::spawn::label(spawn_key), w));
            ui.dummy([1.0, 8.0 * k]);

            // Меняет не тайминг, а смысл награды целиком, поэтому стоит первой.
            if ui.checkbox(t("Ally###rspawnally"), &mut spawn_ally) {
                applied = true;
                touched = true;
            }
            ui.dummy([1.0, 4.0 * k]);

            // Значение применяется КАЖДЫЙ кадр, а в файл уходит по отпусканию.
            // Иначе ползунок «убегает в ноль» (жалоба 2026-08-20): пока его
            // тянут, локальная переменная пересоздаётся из ещё не изменённого
            // действия.
            ui.text_disabled(clip_to(ui, t("Appears in, s"), w));
            let mut delay = f32::from(spawn_delay);
            ui.set_next_item_width(w);
            if ui.slider_config("###rdelay", 0.0, 300.0).display_format("%.0f").build(&mut delay) {
                spawn_delay = delay as u16;
                applied = true;
            }
            touched |= ui.is_item_deactivated_after_edit();

            ui.dummy([1.0, 4.0 * k]);
            // Та же перезарядка, что у эффекта: сколько эту награду нельзя
            // купить снова. Пока она идёт, награда выключена и на Twitch.
            ui.text_disabled(clip_to(ui, t("Cooldown, s"), w));
            let mut cd = f32::from(spawn_cooldown);
            ui.set_next_item_width(w);
            if ui.slider_config("###rspawncd", 0.0, 1800.0).display_format("%.0f").build(&mut cd) {
                spawn_cooldown = cd as u16;
                applied = true;
            }
            touched |= ui.is_item_deactivated_after_edit();

            ui.dummy([1.0, 4.0 * k]);
            // Своё у каждой награды: «медведь на минуту» и «Маления на десять
            // секунд» - разные покупки за разные деньги. Общей настройки
            // больше нет (запрос 2026-09-07).
            ui.text_disabled(clip_to(ui, t("Lifetime, s"), w));
            let mut ttl =
                f32::from(if spawn_ttl == 0 { rewards::default_ttl_secs(spawn_ally) } else { spawn_ttl });
            ui.set_next_item_width(w);
            if ui.slider_config("###rspawnttl", 5.0, 1800.0).display_format("%.0f").build(&mut ttl) {
                spawn_ttl = ttl as u16;
                applied = true;
            }
            touched |= ui.is_item_deactivated_after_edit();
        });

        ui.same_line_with_spacing(0.0, 10.0 * k);
        // Группа, а не второе дочернее окно: поиску нужен общий с плиткой
        // курсор ввода, а списку - свой скролл, и вложенное окно ради этого
        // ни к чему.
        ui.group(|| {
            let row_h = ui.frame_height() + 6.0 * k;
            ui.set_next_item_width(list_w * 0.5);
            let _ = ui
                .input_text("###spawn_search", &mut search)
                .hint(t("Search, or I-IV"))
                .build();

            let mut bosses_only = SPAWN_BOSSES_ONLY.load(Ordering::Relaxed);
            ui.same_line_with_spacing(0.0, 10.0 * k);
            if ui.checkbox(t("bosses"), &mut bosses_only) {
                SPAWN_BOSSES_ONLY.store(bosses_only, Ordering::Relaxed);
            }

            let mut shown: Vec<&crate::spawn::SpawnEntry> =
                crate::spawn::SPAWN_TABLE.iter().filter(|e| e.matches(&search, bosses_only)).collect();
            // От I до IV (запрос 2026-08-24) - в самой таблице порядок по
            // тематике («обычные, мини-боссы, легендарные»), не по ступени.
            // Сортировка стабильна, поэтому внутри одной ступени тематический
            // порядок не ломается.
            shown.sort_by_key(|e| e.tier);
            // Награды «случайный ...» - те же строки списка, только конкретное
            // существо выбирается в момент покупки. По ступени ищутся так же,
            // как враги: «II» отдаёт «Случайный: средний».
            let needle = search.trim().to_lowercase();
            let tier_needle = crate::spawn::tier_of_needle(&search);
            let random: Vec<&crate::spawn::RandomPick> = crate::spawn::RANDOM_PICKS
                .iter()
                .filter(|p| !bosses_only || p.tier == Some(crate::spawn::Tier::Boss))
                .filter(|p| match tier_needle {
                    Some(tier) => p.tier == Some(tier),
                    None => {
                        needle.is_empty()
                            || crate::spawn::label(p.key).to_lowercase().contains(&needle)
                            || p.key.contains(&needle)
                    }
                })
                .collect();

            // Сколько осталось после фильтра: в таблице под две сотни строк, и
            // без счётчика непонятно, поиск сузил список или в нём столько и
            // было.
            ui.same_line_with_spacing(0.0, 10.0 * k);
            ui.text_disabled(format!("{} / {}", shown.len(), crate::spawn::SPAWN_TABLE.len()));

            // Список в своём окне со скроллом: сотня строк иначе выдавила бы
            // кнопки «Проверить» и «Удалить» за низ плитки.
            ui.child_window("###spawn_list").size([list_w, h - row_h]).always_use_window_padding(true).build(|| {
                if shown.is_empty() && random.is_empty() {
                    ui.text_disabled(t("nothing found"));
                }
                // Случайные - первыми и во всю ширину: подписи у них длиннее
                // имён врагов («Случайный: средний (II)»), и в столбце они не
                // помещались (жалоба 2026-08-23).
                let room = (ui.content_region_avail()[0] - ui.frame_height() - 12.0 * k).max(40.0);
                for p in &random {
                    let mut chosen = spawn_key == p.key;
                    let text = clip_to(ui, &crate::spawn::label(p.key), room);
                    if ui.radio_button(format!("{text}###spawn_{}", p.key), &mut chosen, true)
                        && spawn_key != p.key
                    {
                        spawn_key = p.key;
                        touched = true;
                    }
                }
                if !random.is_empty() && !shown.is_empty() {
                    ui.separator();
                }
                // Враги - одним столбцом сверху вниз. Подпись обязана
                // поместиться: ImGui её не переносит и не обрезает, длинное имя
                // рисуется поверх соседнего (скриншот 2026-08-23).
                for e in &shown {
                    let mut chosen = spawn_key == e.key;
                    let text = clip_to(
                        ui,
                        &format!("{} · {}", crate::spawn::label(e.key), e.tier.label()),
                        room,
                    );
                    if ui.radio_button(format!("{text}###spawn_{}", e.key), &mut chosen, true)
                        && spawn_key != e.key
                    {
                        spawn_key = e.key;
                        touched = true;
                    }
                }
            });
        });
    }

    if touched || applied {
        x.rewards[index].action = match kind {
            // Ноль удержания - обычное нажатие, а не отдельный тип: формат
            // файла наград (`Action::Press`/`Action::Hold`) этим не тронут,
            // мержится только выбор в интерфейсе.
            Kind::Press if hold_ms == 0 => Action::Press { key },
            Kind::Press => Action::Hold { key, duration_ms: hold_ms },
            Kind::Spawn => Action::SpawnEnemy {
                key: spawn_key,
                delay_secs: spawn_delay,
                cooldown_secs: spawn_cooldown,
                ally: spawn_ally,
                ttl_secs: spawn_ttl,
            },
            Kind::Effect => Action::Effect {
                key: effect_key,
                secs: effect_secs,
                in_boss: effect_in_boss,
                cooldown_secs: effect_cooldown,
            },
        };
        // Название придумываем за стримера: пустая награда-спавн получает
        // «Спавн Маления», и на Twitch её можно создавать сразу.
        //
        // Подпись в HUD не трогаем - пустая, она и так берётся из названия
        // (`RewardEntry::caption`), а заполненная означала бы, что стример
        // хотел там другое.
        // Своё имя переписываем, набранное руками - никогда. Ушли со спавна на
        // клавишу - «Спавн Маления» станет «Нажать W», а не останется враньём.
        let e = &mut x.rewards[index];
        let now = auto_title(&e.action);
        if now != e.reward_title && adopt_spawn_title(&e.reward_title, &was) {
            e.reward_title = now;
            x.rewards_dirty = true;
        }
        x.rewards_dirty |= touched;
    }
}

/// Тип действия награды. На уровне модуля, а не внутри `reward_action`:
/// `auto_title` рядом с ним покрыт тестом, а правило «какое имя считать
/// своим» ломалось уже дважды за день.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Press,
    Spawn,
    Effect,
}

/// Выбор типа действия равными пилюлями в ряд, вместо голых радиокнопок. Тот
/// же язык клика, что и у плиток награды в списке - `invisible_button` за
/// наведение и клик, `card_frame` за подложку, только без анимации: это не
/// отдельная карточка, а быстрый переключатель, снап без сглаживания читается
/// для него так же чисто.
///
/// **`same_line` тут не годится, и это стоило отдельной жалобы (2026-08-24:
/// «кнопки поплыли, накладываются друг на друга»).** Порядок вызовов внутри
/// сегмента задан жёстко: `invisible_button` первым (в ImGui hover достаётся
/// тому, кто попросил раньше в кадре), `card_frame` за ним (иначе фон накрыл
/// бы текст), текст последним. А `ItemSize` текста перезаписывает
/// `CursorPosPrevLine` его собственным правым краем - текст центрирован, то
/// есть край заметно левее края пилюли, - и `same_line` отсчитывал следующую
/// пилюлю от него. Ошибка копилась с каждым сегментом.
///
/// Поэтому позиция каждого сегмента считается арифметически от `origin`, а
/// курсор ведётся руками - тот же приём и та же причина, что у сетки тайлов в
/// `reward_list`.
///
/// Возвращает новый тип, если в этом кадре кликнули по ДРУГОЙ пилюле, чтобы
/// вызывающий код мог сам решить, что делать с побочными эффектами смены типа
/// (запомнить старое авто-имя, снять захват клавиши).
fn kind_picker(ui: &Ui, accent: [f32; 4], k: f32, w: f32, current: Kind) -> Option<Kind> {
    const OPTIONS: [Kind; 3] = [Kind::Press, Kind::Spawn, Kind::Effect];
    let gap = 8.0 * k;
    let seg_w = ((w - gap * (OPTIONS.len() as f32 - 1.0)) / OPTIONS.len() as f32).max(1.0);
    let h = ui.frame_height() + 6.0 * k;
    let origin = ui.cursor_screen_pos();
    let mut picked = None;

    for (i, variant) in OPTIONS.iter().enumerate() {
        let label = match variant {
            Kind::Press => t("Tap"),
            Kind::Spawn => t("Spawn"),
            Kind::Effect => t("Effect"),
        };
        let pos = [origin[0] + i as f32 * (seg_w + gap), origin[1]];
        ui.set_cursor_screen_pos(pos);
        let selected = current == *variant;
        let clicked = ui.invisible_button(format!("###kind{i}"), [seg_w, h]);
        let hovered = ui.is_item_hovered();
        if clicked && !selected {
            picked = Some(*variant);
        }
        let (fill, border, text_col) = if selected {
            (
                crate::overlay::with_alpha(accent, 0.32),
                crate::overlay::with_alpha(accent, 0.9),
                [1.0, 0.97, 0.88, 1.0],
            )
        } else if hovered {
            (
                crate::overlay::with_alpha(accent, 0.16),
                crate::overlay::with_alpha(accent, 0.55),
                ui.style_color(StyleColor::Text),
            )
        } else {
            (
                crate::overlay::with_alpha(accent, 0.06),
                crate::overlay::with_alpha(accent, 0.4),
                ui.style_color(StyleColor::TextDisabled),
            )
        };
        crate::overlay::card_frame(pos, [pos[0] + seg_w, pos[1] + h], fill, border, 6.0 * k);

        let shown = clip_to(ui, label, (seg_w - 10.0 * k).max(10.0));
        let tsz = ui.calc_text_size(&shown);
        let tx = pos[0] + ((seg_w - tsz[0]) * 0.5).max(0.0);
        let ty = pos[1] + ((h - tsz[1]) * 0.5).max(0.0);
        ui.set_cursor_screen_pos([tx, ty]);
        ui.text_colored(text_col, &shown);
    }

    ui.set_cursor_screen_pos([origin[0], origin[1] + h]);
    ui.dummy([1.0, 1.0]);
    picked
}

/// Как награда называлась бы сама. Есть у ВСЕХ четырёх типов действия: у
/// нажатия и удержания оно появилось по прямому запросу 2026-08-25, до этого
/// такая награда оставалась «без названия» и её нельзя было создать на Twitch.
///
/// Считается из самого действия, а не из выбора в интерфейсе: то, каким имя
/// было ДО правки, - это `auto_title` от нетронутого `Action`, и брать его
/// больше неоткуда не надо.
fn auto_title(action: &Action) -> String {
    match action {
        Action::Press { key } => format!("{} {}", t("Tap"), rewards::key_label(*key)),
        Action::Hold { key, .. } => format!("{} {}", t("Hold"), rewards::key_label(*key)),
        Action::SpawnEnemy { key, ally, .. } => spawn_title(key, *ally),
        Action::Effect { key, .. } => crate::effects::label(key),
    }
}

/// Название по умолчанию для награды-спавна: «Спавн Маления».
///
/// У «случайных» подпись уже сама себе название - «Спавн Случайный враг»
/// читалось бы как опечатка.
fn spawn_title(key: &str, ally: bool) -> String {
    let label = crate::spawn::label(key);
    // Через тире, а не «Союзник Маления»: имя в родительном падеже читалось бы
    // как «чей-то союзник». Приставка нужна и «случайному» тоже - без неё две
    // награды на одного зверя назывались бы одинаково, а Twitch требует
    // уникальных названий.
    if ally {
        return format!("{} - {label}", t("Ally"));
    }
    // У «случайного» подпись уже готовое название («Случайный босс»), и
    // «Спавн Случайный босс» читалось бы косо.
    if crate::spawn::random_pick(key).is_some() {
        return label;
    }
    format!("{} {label}", t("Spawn"))
}

/// Что эффект делает, человеческим языком. Пусто - название говорит само за
/// себя, и лишней строки под ползунками не появляется.
///
/// `text_wrapped` внутри плитки обязан знать границу переноса - сам он тянется
/// до края ОКНА (правило из шапки модуля). Внутри дочернего окна колонки эта
/// граница и есть её ширина.
fn about(ui: &Ui, effect_key: &str) {
    let text = crate::effects::about(effect_key);
    if text.is_empty() {
        return;
    }
    let wrap = ui.push_text_wrap_pos_with_pos(ui.content_region_avail()[0]);
    ui.text_disabled(text);
    wrap.end();
}

/// Можно ли переписать название награды при смене врага.
///
/// Пустое - можно, за тем всё и заведено. Совпадающее с авто-именем
/// ПРЕДЫДУЩЕГО врага - тоже: иначе выбрал Маления, передумал, выбрал Годрика,
/// а на Twitch уехала бы «Спавн Маления», спавнящая Годрика.
///
/// Всё остальное набрано руками, и трогать его нельзя.
fn adopt_spawn_title(current: &str, previous_auto: &str) -> bool {
    let current = current.trim();
    current.is_empty() || current == previous_auto
}

/// Состояние карточки с прошлого кадра. См. `remembered` - приём и причина те
/// же, только ключ здесь номер награды, а не заголовок.
fn row_metrics(id: u32) -> RowAnim {
    ROW_H
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, a)| *a)
        .unwrap_or_default()
}

fn remember_row(id: u32, a: RowAnim) {
    let mut rows = ROW_H.lock().unwrap_or_else(|e| e.into_inner());
    match rows.iter_mut().find(|(k, _)| *k == id) {
        Some(slot) => *slot = (id, a),
        None => {
            // Удалённая награда оставляет здесь свою запись навсегда, а номера
            // не переиспользуются: за долгую сессию с правкой списка их
            // набежало бы сколько угодно. Сбрасываем целиком - цена одна
            // мигнувшая высота, и та у наград, которых уже нет.
            if rows.len() >= 64 {
                rows.clear();
            }
            rows.push((id, a));
        }
    }
}

fn describe(action: &Action) -> String {
    match action {
        Action::Press { key } => format!("{} {}", t("tap"), rewards::key_label(*key)),
        Action::Hold { key, duration_ms } => format!(
            "{} {} {:.1} {}",
            t("hold"),
            rewards::key_label(*key),
            *duration_ms as f32 / 1000.0,
            t("s")
        ),
        Action::SpawnEnemy { key, delay_secs, cooldown_secs, ally, .. } => {
            let verb = if *ally { t("ally") } else { t("spawn") };
            let mut what = format!("{verb} {}", crate::spawn::label(key));
            if *delay_secs > 0 {
                what.push_str(&format!(", {} {delay_secs} {}", t("in"), t("s")));
            }
            if *cooldown_secs > 0 {
                what.push_str(&format!(", {} {cooldown_secs} {}", t("cooldown"), t("s")));
            }
            what
        }
        Action::Effect { key, secs, in_boss, cooldown_secs } => {
            let mut what = crate::effects::label(key);
            if !in_boss {
                what.push_str(t(", not in boss fights"));
            }
            // Срок показываем только у временных: у разового его нет вовсе, и
            // «Полное лечение, 0 с» читалось бы как поломка.
            let secs = if *secs > 0 { *secs } else { crate::effects::default_secs(key) };
            if crate::effects::default_secs(key) > 0 {
                what = format!("{what}, {secs} {}", t("s"));
            }
            if *cooldown_secs > 0 {
                what.push_str(&format!(", {} {cooldown_secs} {}", t("cooldown"), t("s")));
            }
            what
        }
    }
}

// ---------------------------------------------------------------------------
// Раздел: прочее
// ---------------------------------------------------------------------------

fn page_other(ui: &Ui, g: &mut Grid, x: &mut Ctx) {
    let p = PAGE_OTHER;
    // Список строится из файлов в папке `locale` рядом с DLL: положил свой
    // файл - появилась ещё одна кнопка, пересобирать мод не нужно.
    g.card(ui, p, t("Language"), "язык перевод локализация language locale translation", |_| {
        let langs = i18n::languages();
        let rows = std::iter::once((i18n::AUTO, t("Same as the game")))
            .chain(langs.iter().map(|l| (l.code, l.name)));
        for (code, label) in rows {
            let mut chosen = x.c.lang.eq_ignore_ascii_case(code);
            if ui.radio_button(label, &mut chosen, true) && !x.c.lang.eq_ignore_ascii_case(code) {
                x.c.lang = code.to_string();
                // Применяем сразу, а не при следующей загрузке: иначе «поменял
                // язык, ничего не произошло» выглядит поломкой.
                i18n::apply(code);
                x.changes.push(("lang", code.to_string()));
            }
        }
        if langs.is_empty() {
            hint(ui, t("No .ini files in the locale folder next to the DLL."));
        }
    });

    g.card(ui, p, t("When to hide the HUD"), "прятать катсцена меню hide cutscene menu", |_| {
        check(ui, &mut x.changes, t("During cutscenes"), "hide_in_cutscene", &mut x.c.hide_in_cutscene);
        check(ui, &mut x.changes, t("In the game menu"), "hide_in_menu", &mut x.c.hide_in_menu);
    });

    g.card(ui, p, t("Bosses"), "боссы список неубитые остались bosses list remaining", |_| {
        if button(ui, t("Boss list")) {
            BOSS_LIST_OPEN.store(true, Ordering::Relaxed);
        }
        ui.same_line();
        let learned = crate::bosses::learned_count();
        ui.text_disabled(format!("{}: {learned}", t("names")));
        hint(
            ui,
            t("Names picked up during a fight."),
        );
        if learned > 0 && button(ui, t("Forget the names")) {
            crate::bosses::forget_learned();
        }
    });

    g.card(ui, p, t("Summons"), "призванные спавн дебаг summons debug spawn", |_| {
        check(ui, &mut x.changes, t("Debug spawner"), "debug_spawn", &mut x.c.debug_spawn);
        hint(ui, t("No limit on how many enemies can be in the world. They do not follow you and may come out invisible. Allies always come through the ash."));
    });

    g.card(ui, p, t("Diagnostics"), "отладка диагностика debug spawn", |_| {
        check(ui, &mut x.changes, t("Debug window"), "debug", &mut x.c.debug);
    });
}

/// Открыть или закрыть список боссов. Зовётся по горячей клавише из `render`.
pub fn toggle_boss_list() {
    let now = BOSS_LIST_OPEN.load(Ordering::Relaxed);
    BOSS_LIST_OPEN.store(!now, Ordering::Relaxed);
}

/// Список неубитых боссов: где они и далеко ли.
///
/// Данные тянутся из `bosses::rows()` - он сам не чаще раза в секунду ходит в
/// парамы, поэтому звать его каждый кадр можно.
pub fn boss_list_window(ui: &Ui, c: &mut Config) -> Changes {
    let mut changes = Changes::new();
    // Появление и уход - как у окна редактора наград: вниз быстрее, чем вверх.
    let open = BOSS_LIST_OPEN.load(Ordering::Relaxed);
    let dt = ui.io().delta_time.clamp(0.0, 0.1);
    let target = if open { 1.0 } else { 0.0 };
    let fade = {
        let mut slot = BOSS_LIST_FADE.lock().unwrap_or_else(|e| e.into_inner());
        *slot = approach(*slot, target, dt, if target > *slot { 0.15 } else { 0.12 });
        *slot
    };
    if fade < 0.01 {
        return changes;
    }

    let k = scale(ui);
    let accent = c.accent_color;
    let rows = crate::bosses::rows(c.boss_list_radius);
    let here_only = BOSS_LIST_HERE.load(Ordering::Relaxed);
    let mut needle = BOSS_SEARCH.lock().unwrap_or_else(|e| e.into_inner());

    let show_killed = BOSS_LIST_KILLED.load(Ordering::Relaxed);
    let only_remembrance = BOSS_LIST_REMEMBRANCE.load(Ordering::Relaxed);
    let alive = rows.iter().filter(|r| !r.killed).map(|r| r.count).sum::<usize>();
    let total = rows.iter().map(|r| r.count).sum::<usize>();

    // Окно живёт само по себе, без настроек, поэтому тему и шрифт пушит себе
    // само - иначе по F8 оно выходило бы стоковым серым ImGui.
    let font = crate::overlay::settings_font();
    let pushed = !font.is_null();
    if pushed {
        unsafe { hudhook::imgui::sys::igPushFont(font as *mut _) };
    }
    let _colors = theme(ui, accent, c.value_color, c.label_color);
    let _vars = metrics(ui, k);

    let mut window_open = true;
    let flags = if fade < 0.999 { WindowFlags::NO_MOUSE_INPUTS } else { WindowFlags::empty() };
    let _fade = ui.push_style_var(StyleVar::Alpha(ui.clone_style().alpha * fade.clamp(0.0, 1.0)));
    // Счёт в заголовке, а не строкой внутри: окно часто сворачивают в полоску,
    // и тогда виден только он.
    ui.window(format!(
        "{} - {}/{}###boss_list",
        t("Boss list"),
        total - alive,
        total
    ))
        .flags(flags)
        .opened(&mut window_open)
        .size([420.0 * k, 480.0 * k], Condition::FirstUseEver)
        .build(|| {
            let inner = ui.content_region_avail()[0];

            let mut only = here_only;
            if ui.checkbox(t("Nearby"), &mut only) {
                BOSS_LIST_HERE.store(only, Ordering::Relaxed);
            }
            // Ползунок радиуса стоит тут, а не в настройках: он настраивает
            // ровно эту галочку, и крутить его надо глядя на результат
            // (запрос 2026-09-07). `rows()` держит радиус в условии кэша,
            // поэтому список отвечает сразу, а не через секунду.
            //
            // Показывается только при включённой галочке: без неё радиус ни на
            // что не влияет, а строка фильтров и так впритык по ширине.
            if only {
                ui.same_line();
                ui.set_next_item_width(80.0 * k);
                ui.slider_config("###blradius", 200.0, 8000.0)
                    .display_format(step_of(8000.0))
                    .build(&mut c.boss_list_radius);
                if ui.is_item_hovered() {
                    ui.tooltip_text(t("Radius, m"));
                }
                if ui.is_item_deactivated_after_edit() {
                    changes.push(("boss_list_radius", format!("{}", c.boss_list_radius)));
                }
            }
            ui.same_line();
            let mut killed = show_killed;
            if ui.checkbox(t("Killed"), &mut killed) {
                BOSS_LIST_KILLED.store(killed, Ordering::Relaxed);
            }
            ui.same_line();
            let mut rem = only_remembrance;
            if ui.checkbox(t("Remembrance"), &mut rem) {
                BOSS_LIST_REMEMBRANCE.store(rem, Ordering::Relaxed);
            }
            ui.set_next_item_width(inner);
            ui.input_text("##search", &mut needle)
                .hint(t("search"))
                .build();

            let q = needle.to_lowercase();
            let shown: Vec<&crate::bosses::Row> = rows
                .iter()
                .filter(|r| show_killed || !r.killed)
                .filter(|r| !only || r.here)
                .filter(|r| !only_remembrance || r.remembrance)
                .filter(|r| {
                    q.is_empty()
                        || r.name.to_lowercase().contains(&q)
                        || r.place.to_lowercase().contains(&q)
                })
                .collect();

            // Сколько локаций ещё не зачищено - по этому числу видно, много
            // ли бегать, а не только сколько боссов осталось.
            let left_places = {
                let mut p: Vec<&str> =
                    rows.iter().filter(|r| !r.killed).map(|r| r.place.as_str()).collect();
                p.sort_unstable();
                p.dedup();
                p.len()
            };
            ui.text_disabled(format!(
                "{}: {alive} / {total}  |  {}: {left_places}",
                t("remaining"),
                t("places")
            ));
            ui.separator();

            if rows.is_empty() {
                ui.text_disabled(t("The game data is still loading."));
                return;
            }
            if shown.is_empty() {
                if only {
                    // Радиус называется прямо здесь: иначе пустой список
                    // читается как поломка, а не как «слишком узко».
                    hint(
                        ui,
                        &format!(
                            "{} {:.0} {}",
                            t("No bosses within"),
                            c.boss_list_radius,
                            t("m. Raise the radius in the settings.")
                        ),
                    );
                } else {
                    ui.text_disabled(t("Nothing found."));
                }
                return;
            }

            ui.child_window("##boss_rows").build(|| {
                // Полоса прокрутки съедает правый край, и метры уезжали под
                // неё (жалоба 2026-08-27). Считаем ширину контента, а не окна.
                let body = ui.content_region_avail()[0];
                let mut i = 0usize;
                while i < shown.len() {
                    let place = shown[i].place.as_str();
                    let group: Vec<&crate::bosses::Row> =
                        shown[i..].iter().copied().take_while(|o| o.place == place).collect();
                    i += group.len();

                    let all: usize = group.iter().map(|o| o.count).sum();
                    let done: usize =
                        group.iter().filter(|o| o.killed).map(|o| o.count).sum();
                    let folded = boss_group_folded(place);
                    // Без анимации: список листают, а не разглядывают, и
                    // плавное раскрытие читалось как задержка (жалоба
                    // 2026-08-28).
                    let open_t = if folded { 0.0 } else { 1.0 };

                    let head_h = ui.text_line_height() + 8.0 * k;
                    let top = ui.cursor_screen_pos();
                    let hovered = ui.is_mouse_hovering_rect(top, [top[0] + body, top[1] + head_h]);

                    // Шапка локации: своя подложка, полоса прогресса по её
                    // ширине и маркер сворачивания. Прозрачная строка не
                    // читалась как то, по чему кликают.
                    let done_frac = if all == 0 { 0.0 } else { done as f32 / all as f32 };
                    crate::overlay::card_frame(
                        top,
                        [top[0] + body, top[1] + head_h],
                        crate::overlay::with_alpha(accent, if hovered { 0.16 } else { 0.09 }),
                        crate::overlay::with_alpha(accent, 0.0),
                        4.0 * k,
                    );
                    if done_frac > 0.0 {
                        crate::overlay::card_frame(
                            top,
                            [top[0] + body * done_frac, top[1] + head_h],
                            crate::overlay::with_alpha(accent, 0.22),
                            crate::overlay::with_alpha(accent, 0.0),
                            4.0 * k,
                        );
                    }
                    if ui.invisible_button(format!("##fold{place}"), [body, head_h]) {
                        toggle_boss_group(place);
                    }

                    let count = if show_killed {
                        format!("{done}/{all}")
                    } else {
                        format!("{all}")
                    };
                    let count_w = ui.calc_text_size(&count)[0];
                    ui.set_cursor_screen_pos([top[0] + 8.0 * k, top[1] + 4.0 * k]);
                    let mid = ui.cursor_screen_pos();
                    crate::overlay::caret(
                        [mid[0], mid[1] + ui.text_line_height() * 0.5],
                        ui.text_line_height() * 0.6,
                        open_t > 0.5,
                        crate::overlay::with_alpha(accent, 0.9),
                    );
                    ui.set_cursor_screen_pos([top[0] + 20.0 * k, top[1] + 4.0 * k]);
                    ui.text_colored(accent, clip_to(ui, place, body - count_w - 34.0 * k));
                    ui.same_line_with_pos(0.0);
                    ui.set_cursor_screen_pos([top[0] + body - count_w - 8.0 * k, top[1] + 4.0 * k]);
                    ui.text_disabled(&count);
                    ui.set_cursor_screen_pos([top[0], top[1] + head_h + 2.0 * k]);

                    // Свёрнутая группа не рисует строк вовсе - список из
                    // полусотни локаций иначе не пролистать.
                    if open_t > 0.01 {
                        let _alpha =
                            ui.push_style_var(StyleVar::Alpha(ui.clone_style().alpha * open_t));
                        for r in &group {
                            let dist = match r.dist {
                                // Та же приписка высоты, что на панели: цифра
                                // расстояния без неё врёт про босса этажом ниже.
                                Some(d) => format!(
                                    "{d:.0}{}{}",
                                    t("m"),
                                    crate::overlay::height_mark(r.dy.unwrap_or(0.0))
                                ),
                                None => String::new(),
                            };
                            let dist_w = ui.calc_text_size(&dist)[0];
                            let name = if r.count > 1 {
                                format!("{} x{}", r.name, r.count)
                            } else {
                                r.name.clone()
                            };
                            let row_top = ui.cursor_screen_pos();
                            ui.set_cursor_screen_pos([row_top[0] + 20.0 * k, row_top[1]]);
                            let text = clip_to(ui, &name, body - dist_w - 34.0 * k);
                            if r.killed {
                                ui.text_disabled(text);
                            } else {
                                ui.text(text);
                            }
                            if !dist.is_empty() {
                                ui.same_line_with_pos(0.0);
                                ui.set_cursor_screen_pos([
                                    row_top[0] + body - dist_w - 8.0 * k,
                                    row_top[1],
                                ]);
                                ui.text_disabled(&dist);
                            }
                        }
                    }
                    ui.dummy([1.0, 6.0 * k]);
                }
            });
        });
    if !window_open {
        BOSS_LIST_OPEN.store(false, Ordering::Relaxed);
    }
    if pushed {
        unsafe { hudhook::imgui::sys::igPopFont() };
    }
    changes
}

fn boss_group_folded(place: &str) -> bool {
    BOSS_FOLDED.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|p| p == place)
}

fn toggle_boss_group(place: &str) {
    let mut folded = BOSS_FOLDED.lock().unwrap_or_else(|e| e.into_inner());
    match folded.iter().position(|p| p == place) {
        Some(i) => {
            folded.swap_remove(i);
        }
        None => folded.push(place.to_string()),
    }
}

/// Открыт ли список - от этого зависят курсор и захват ввода.
pub fn boss_list_open() -> bool {
    BOSS_LIST_OPEN.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Мелкие виджеты
// ---------------------------------------------------------------------------

/// Выбор корпуса. `id` разводит два независимых набора: у панели в игре и у
/// виджета в OBS он свой - поверх геймплея и поверх сцены OBS удачным
/// оказывается разное.
///
/// Яркость живёт здесь же, а не в «Цвете»: и рамка, и полоса слева красятся
/// именно ей, и с нулевой яркостью выбранный корпус просто не рисуется. Раньше
/// ползунок стоял в другой плитке, и связь была не видна - «где полоса?» при
/// яркости в нуле (жалоба 2026-08-20).
fn chassis_picker(ui: &Ui, key: ChassisKeys, c: &mut Config, changes: &mut Changes) {
    let (current, opacity, rounding, width) = (key.style)(c);
    for (variant, label) in [
        (PanelStyle::Frame, t("Frame all around")),
        (PanelStyle::Bare, t("No frame, no fill")),
        (PanelStyle::Bar, t("Bar on the left")),
        (PanelStyle::BarRight, t("Bar on the right")),
    ] {
        let mut chosen = *current == variant;
        if ui.radio_button(format!("{label}###{}{}", key.id, variant.as_key()), &mut chosen, true)
            && *current != variant
        {
            *current = variant;
            changes.push((key.style_key, variant.as_key().to_string()));
            // Выбрать корпус, которого не будет видно, никто не хочет:
            // поднимаем яркость до значения по умолчанию, если её увели в ноль
            // при другом корпусе. Ползунок рядом - убавить обратно можно сразу.
            if variant != PanelStyle::Bare && *opacity < 0.05 {
                *opacity = 0.85;
                changes.push((key.opacity_key, opacity.to_string()));
            }
        }
    }
    // Скругление есть у плиты при любом корпусе, кроме `bare` - там плиты нет
    // вовсе. Толщина линии - только там, где линия рисуется.
    if *current != PanelStyle::Bare {
        slider(ui, changes, t("Corner radius"), key.rounding_key, rounding, 0.0, 24.0);
        slider(ui, changes, t("Line width"), key.width_key, width, 0.5, 8.0);
        slider(ui, changes, t("Brightness"), key.opacity_key, opacity, 0.0, 1.0);
    }
}

/// Какие поля конфига правит `chassis_picker`. Структурой, а не восемью
/// параметрами: набор один и тот же у панели и у виджета, различаются только
/// имена ключей и то, из каких полей `Config` их брать.
struct ChassisKeys {
    /// Разводит два одинаковых набора радиокнопок в одном окне.
    id: &'static str,
    style: fn(&mut Config) -> (&mut PanelStyle, &mut f32, &mut f32, &mut f32),
    style_key: &'static str,
    opacity_key: &'static str,
    rounding_key: &'static str,
    width_key: &'static str,
}

const CHASSIS_OVERLAY: ChassisKeys = ChassisKeys {
    id: "overlay",
    style: |c| (&mut c.panel_style, &mut c.border_opacity, &mut c.panel_rounding, &mut c.border_width),
    style_key: "panel_style",
    opacity_key: "border_opacity",
    rounding_key: "panel_rounding",
    width_key: "border_width",
};

const CHASSIS_TOAST: ChassisKeys = ChassisKeys {
    id: "toast",
    style: |c| (&mut c.toast_style, &mut c.toast_border_opacity, &mut c.toast_rounding, &mut c.toast_border_width),
    style_key: "toast_style",
    opacity_key: "toast_border_opacity",
    rounding_key: "toast_rounding",
    width_key: "toast_border_width",
};

const CHASSIS_WEB_TOAST: ChassisKeys = ChassisKeys {
    id: "webtoast",
    style: |c| {
        (&mut c.web_toast_style, &mut c.web_toast_border_opacity, &mut c.web_toast_rounding, &mut c.web_toast_border_width)
    },
    style_key: "web_toast_style",
    opacity_key: "web_toast_border_opacity",
    rounding_key: "web_toast_rounding",
    width_key: "web_toast_border_width",
};

const CHASSIS_WEB: ChassisKeys = ChassisKeys {
    id: "web",
    style: |c| {
        (&mut c.web_panel_style, &mut c.web_border_opacity, &mut c.web_panel_rounding, &mut c.web_border_width)
    },
    style_key: "web_panel_style",
    opacity_key: "web_border_opacity",
    rounding_key: "web_panel_rounding",
    width_key: "web_border_width",
};

/// Выбор компоновки. `id` разводит два одинаковых набора радиокнопок: у
/// оверлея и у виджета они настраиваются независимо.
fn layout_picker(ui: &Ui, id: &str, current: &mut Layout, key: &'static str, changes: &mut Changes) {
    // Только названия: пояснение сбоку («подпись слева, значение справа»)
    // в плитку не помещалось, а сам вариант подписан.
    for (variant, label) in [
        (Layout::D, t("List")),
        (Layout::A, t("Two columns")),
        (Layout::B, t("One line")),
    ] {
        let mut chosen = *current == variant;
        if ui.radio_button(format!("{label}##{id}"), &mut chosen, true) && *current != variant {
            *current = variant;
            changes.push((key, variant.as_key().to_string()));
        }
    }
}

/// Обрезка подписи по месту. ImGui подписи виджетов не переносит и не
/// обрезает: то, что не влезло, рисуется поверх соседней плитки - ровно на это
/// пожаловались 2026-08-20. Многоточие - три обычные точки: глифа U+2026 в
/// атласе нет (пункт 4 в шапке).
fn clip_to(ui: &Ui, text: &str, room: f32) -> String {
    if ui.calc_text_size(text)[0] <= room {
        return text.to_string();
    }
    let dots = ui.calc_text_size("...")[0];
    let mut end = 0;
    for (i, _) in text.char_indices() {
        if ui.calc_text_size(&text[..i])[0] + dots > room {
            break;
        }
        end = i;
    }
    format!("{}...", text[..end].trim_end())
}

/// Строка «ползунок, справа подпись»: ползунку остаток плитки, подписи - что
/// осталось ей. Возвращает готовую подпись с id и ширину виджета.
///
/// Id держится за `key`, а не за текст: обрезанная подпись меняется от ширины
/// окна, и без этого ImGui считал бы виджет новым при каждом изменении
/// размера, теряя его состояние. Именно `###`: `##` прячет текст от показа, но
/// в id он всё равно попадает.
fn labeled(ui: &Ui, label: &str, key: &str) -> (String, f32) {
    let full = ui.calc_item_width();
    let gap = 10.0;
    // Ползунок уже трети плитки мышью не поймать, поэтому место ему уступает
    // подпись, а не наоборот.
    let min_ctrl = (full * 0.4).max(48.0);
    let room = (full - min_ctrl - gap).max(24.0);
    if ui.calc_text_size(label)[0] <= room {
        let ctrl = (full - ui.calc_text_size(label)[0] - gap).max(min_ctrl);
        return (format!("{label}###{key}"), ctrl);
    }
    // Не влезла - переносим по словам НАД виджетом, а виджет занимает всю
    // ширину. Раньше лишнее резалось многоточием, и длинную подпись нельзя
    // было прочитать вовсе (жалоба со скриншотом 2026-09-08).
    //
    // Границу переноса ставит сама плитка (`Grid`), поэтому `text_wrapped`
    // здесь заворачивает по её краю, а не по краю окна.
    ui.text_wrapped(label);
    (format!("###{key}"), full)
}

/// Докуда показывать образец карточки покупки. Клик по радиокнопке корпуса -
/// событие на один кадр, и без этого образец мелькнул бы и пропал.
static TOAST_PREVIEW_UNTIL: Mutex<Option<std::time::Instant>> = Mutex::new(None);
/// То же самое для оверлея союзников.
static ALLY_PREVIEW_UNTIL: Mutex<Option<std::time::Instant>> = Mutex::new(None);
const TOAST_PREVIEW_LINGER: std::time::Duration = std::time::Duration::from_millis(2500);

/// Подпись группы внутри плитки: акцентом и линейкой под ней - тот же язык,
/// что у заголовков плиток и секций раскрытой награды. Нужна там, где в одной
/// плитке настраиваются разные вещи и сплошной список ползунков не читается.
fn group(ui: &Ui, c: &Config, name: &str) {
    // `content_region_avail` здесь НЕ годится: он считает остаток до правого
    // края ОКНА, а не плитки, и в два столбца линейка уходила через весь
    // раздел поверх соседней плитки (скриншот 2026-09-07). Ширину плитки
    // знает только `Grid`, и он кладёт её в item width - оттуда её берёт и
    // `fit`.
    let w = ui.calc_item_width();
    ui.dummy([1.0, 5.0]);
    ui.text_colored(crate::overlay::with_alpha(c.accent_color, 0.70), name);
    ui.dummy([1.0, 1.0]);
    crate::overlay::rule_line(
        ui.cursor_screen_pos(),
        w,
        crate::overlay::with_alpha(c.accent_color, 0.18),
    );
    ui.dummy([1.0, 5.0]);
}

/// Пояснение под элементом: серым и с переносом по ширине плитки.
fn hint(ui: &Ui, text: &str) {
    let col = ui.push_style_color(StyleColor::Text, ui.clone_style()[StyleColor::TextDisabled]);
    ui.text_wrapped(text);
    col.end();
}

/// Кнопка без подсветки края.
///
/// `FrameBorderSize` в теме включён ради тёмных плашек - галочек, полей и
/// ползунков, которые на прозрачном окне иначе сливаются с игрой. Кнопки и
/// заголовки светлые сами по себе, и контур на них выглядит лишней рамкой
/// (жалоба 2026-08-20), поэтому вокруг них он гасится.
fn button(ui: &Ui, label: impl AsRef<str>) -> bool {
    let _no_border = ui.push_style_var(StyleVar::FrameBorderSize(0.0));
    ui.button(label)
}

const PILL_PAD_X: f32 = 8.0;
const PILL_PAD_Y: f32 = 3.0;

/// Статус пилюлей - закрашенный скруглённый фон вместо голого цветного текста,
/// тот же язык акцента, что и у плиток в остальном окне (`card_frame`). Ведёт
/// себя как обычный элемент вёрстки: резервирует своё место через `dummy`,
/// поэтому строку до и после можно продолжать `same_line` как всегда.
fn status_pill(ui: &Ui, text: &str, color: [f32; 4], k: f32) {
    let (pad_x, pad_y) = (PILL_PAD_X * k, PILL_PAD_Y * k);
    let sz = ui.calc_text_size(text);
    let p = ui.cursor_screen_pos();
    let (w, h) = (sz[0] + pad_x * 2.0, sz[1] + pad_y * 2.0);
    crate::overlay::card_frame(
        p,
        [p[0] + w, p[1] + h],
        crate::overlay::with_alpha(color, 0.16),
        crate::overlay::with_alpha(color, 0.55),
        h * 0.5,
    );
    ui.set_cursor_screen_pos([p[0] + pad_x, p[1] + pad_y]);
    ui.text_colored(color, text);
    ui.set_cursor_screen_pos(p);
    ui.dummy([w, h]);
}

/// Галочка. Ширина квадратика фиксирована, поэтому подписи достаётся остаток
/// плитки - и обрезается по нему.
fn check(ui: &Ui, changes: &mut Changes, label: &str, key: &'static str, v: &mut bool) {
    let room = (ui.calc_item_width() - ui.frame_height() - 10.0).max(24.0);
    if ui.calc_text_size(label)[0] <= room {
        if ui.checkbox(format!("{label}###{key}"), v) {
            changes.push((key, v.to_string()));
        }
        return;
    }
    // Длинная подпись переносится по словам рядом с квадратиком. Кликается
    // тогда только сам квадратик - у галочки ImGui подпись часть виджета, и
    // отделить её без потери клика нечем.
    if ui.checkbox(format!("###{key}"), v) {
        changes.push((key, v.to_string()));
    }
    ui.same_line();
    ui.text_wrapped(label);
}

/// Шаг ползунка. ImGui округляет значение по формату вывода
/// (`RoundScalarWithFormat`), а стоковый "%.3f" давал тысячные: доли ловились
/// мышью с точностью, которая нигде не нужна. Доли - сотыми (запрос
/// 2026-08-24, было десятыми), всё, что считается в пикселях, секундах или
/// метрах, - целыми.
fn step_of(max: f32) -> &'static str {
    if max <= 10.0 {
        "%.2f"
    } else {
        "%.0f"
    }
}

/// Ползунок, который применяется сразу, а в файл пишется по отпусканию.
/// `is_item_deactivated_after_edit` единственный способ отличить «тянут» от
/// «дотянули», а без этого различия одно движение мышью превращается в сотню
/// перезаписей `.ini` и, для кеглей, в сотню пересборок атласа.
fn slider(ui: &Ui, changes: &mut Changes, label: &str, key: &'static str, v: &mut f32, min: f32, max: f32) {
    let (id, iw) = labeled(ui, label, key);
    ui.set_next_item_width(iw);
    ui.slider_config(id, min, max).display_format(step_of(max)).build(v);
    if ui.is_item_deactivated_after_edit() {
        changes.push((key, format!("{v}")));
    }
}

fn int_slider(ui: &Ui, changes: &mut Changes, label: &str, key: &'static str, v: &mut u32, min: u32, max: u32) {
    let mut n = *v as i32;
    let (id, iw) = labeled(ui, label, key);
    ui.set_next_item_width(iw);
    ui.slider(id, min as i32, max as i32, &mut n);
    *v = n.clamp(min as i32, max as i32) as u32;
    if ui.is_item_deactivated_after_edit() {
        changes.push((key, v.to_string()));
    }
}

fn color(ui: &Ui, changes: &mut Changes, label: &str, key: &'static str, v: &mut [f32; 4]) {
    let (id, iw) = labeled(ui, label, key);
    ui.set_next_item_width(iw);
    ui.color_edit4(id, v);
    // Палитру тоже тянут мышью, поэтому та же логика, что у ползунков.
    if ui.is_item_deactivated_after_edit() {
        changes.push((key, color_to_hex(*v)));
    }
}

/// Ползунок кегля шрифта. В отличие от обычного `slider`, пишет в файл
/// БАЗОВЫЙ (без `ui_scale`) размер: живое поле уже содержит запечённый
/// масштаб (`Config::apply_scale`, как в elden), а на следующей загрузке
/// `Config::load` домножит сохранённое обратно на текущий `ui_scale`. Если
/// сохранить домноженное значение, оно удвоится при перезапуске.
#[allow(clippy::too_many_arguments)]
fn font_slider(
    ui: &Ui,
    changes: &mut Changes,
    label: &str,
    key: &'static str,
    v: &mut f32,
    min: f32,
    max: f32,
    ui_scale: f32,
) {
    let (id, iw) = labeled(ui, label, key);
    ui.set_next_item_width(iw);
    // Целые пиксели: доля кегля не видна ни на одном разрешении.
    ui.slider_config(id, min, max).display_format("%.0f").build(v);
    if ui.is_item_deactivated_after_edit() {
        changes.push((key, format!("{}", *v / ui_scale)));
    }
}

/// Кнопка, покрашенная под текст. ImGui не знает ни ссылок, ни кликабельного
/// текста, поэтому и то и другое строится отсюда.
fn flat_button(ui: &Ui, c: &Config, label: &str) -> bool {
    // Ссылка покрашена под текст, и рамка вокруг неё выдала бы кнопку.
    let _no_border = ui.push_style_var(StyleVar::FrameBorderSize(0.0));
    let tint = ui.push_style_color(StyleColor::Button, [0.0, 0.0, 0.0, 0.0]);
    let hovered = ui.push_style_color(StyleColor::ButtonHovered, crate::overlay::with_alpha(c.accent_color, 0.2));
    let text = ui.push_style_color(StyleColor::Text, c.accent_color);
    let clicked = button(ui, label);
    text.end();
    hovered.end();
    tint.end();
    clicked
}

/// Сам адрес и есть ссылка: переписывать его с экрана руками, сидя в игре,
/// худшее из возможного. Длинный адрес обрезается по плитке, а целиком его
/// показывает подсказка - кликают всё равно по нему целому.
fn link(ui: &Ui, c: &Config, url: &str) {
    let shown = clip_to(ui, url, ui.calc_item_width());
    if flat_button(ui, c, &shown) {
        crate::input::open_url(url);
    }
    if ui.is_item_hovered() {
        ui.tooltip_text(format!("{url}\n{}", t("open in the browser")));
    }
}

/// Адрес для OBS: клик кладёт его в буфер обмена. Ни открывать в браузере, ни
/// набирать руками его не нужно, нужно вставить в источник Browser Source.
///
/// Подпись стоит НАД адресом, а не рядом: рядом они вдвоём не помещались в
/// плитку и налезали на соседнюю.
fn copyable(ui: &Ui, c: &Config, url: &str, what: &str) {
    ui.text_disabled(what);
    let shown = clip_to(ui, url, ui.calc_item_width());
    if flat_button(ui, c, &shown) {
        ui.set_clipboard_text(url);
    }
    if ui.is_item_hovered() {
        ui.tooltip_text(format!("{url}\n{}", t("copy")));
    }
}

#[cfg(test)]
mod tests {
    /// Точка внутри треугольника - по знаку трёх векторных произведений.
    /// Нужна, чтобы проверять НАРИСОВАННОЕ, а не буфер вершин: у прямых
    /// участков обводки внутренних вершин нет вовсе.
    fn in_triangle(q: [f32; 2], t: [[f32; 2]; 3]) -> bool {
        let side = |a: [f32; 2], b: [f32; 2]| (b[0] - a[0]) * (q[1] - a[1]) - (b[1] - a[1]) * (q[0] - a[0]);
        let (d0, d1, d2) = (side(t[0], t[1]), side(t[1], t[2]), side(t[2], t[0]));
        let neg = d0 < 0.0 || d1 < 0.0 || d2 < 0.0;
        let pos = d0 > 0.0 || d1 > 0.0 || d2 > 0.0;
        !(neg && pos)
    }

    use super::*;
    use hudhook::imgui::Context;

    /// Столбцы считаются по ширине окна: узкое - один, широкое - два, очень
    /// широкое - три. На 4K плитка вдвое шире, поэтому столбцов при той же
    /// ширине меньше.
    /// Название придумывается только там, где своего нет: пустое или
    /// оставшееся от прошлого выбора. Набранное руками неприкосновенно.
    #[test]
    fn auto_title_never_overwrites_a_hand_typed_name() {
        assert!(adopt_spawn_title("", "Спавн Маления"));
        assert!(adopt_spawn_title("   ", "Спавн Маления"));
        assert!(adopt_spawn_title("Спавн Маления", "Спавн Маления"), "имя от прошлого выбора");
        assert!(!adopt_spawn_title("Подарочек", "Спавн Маления"));
        assert!(!adopt_spawn_title("Спавн Годрика", "Спавн Маления"), "стример переименовал сам");
    }

    /// Смена типа награды: своё имя переписывается, чужое - никогда.
    ///
    /// Ломалось дважды за день (2026-08-24): сначала при переходе на эффект
    /// сравнивали с НОВЫМ авто-именем вместо прежнего, потом перестали стирать
    /// имя при уходе на «нажать».
    #[test]
    fn switching_the_action_type_only_rewrites_its_own_title() {
        let press = Action::Press { key: Key::Space };
        let hold = Action::Hold { key: Key::W, duration_ms: 1500 };
        let spawn = Action::SpawnEnemy {
            key: crate::spawn::SPAWN_TABLE[0].key,
            delay_secs: 0,
            cooldown_secs: 0,
            ally: false,
            ttl_secs: 0,
        };
        let effect = Action::Effect {
            key: crate::effects::EFFECT_TABLE[0].key,
            secs: 0,
            in_boss: true,
            cooldown_secs: 0,
        };
        // Имя есть у всех четырёх, и у всех разное - иначе «своё» имя одного
        // типа считалось бы своим и у другого.
        let names: Vec<String> = [&press, &hold, &spawn, &effect].map(auto_title).to_vec();
        for (i, a) in names.iter().enumerate() {
            assert!(!a.trim().is_empty(), "у каждого типа своё имя");
            for b in &names[i + 1..] {
                assert_ne!(a, b);
            }
        }

        // Спавн -> эффект: сравнивать надо с именем СПАВНА.
        assert!(adopt_spawn_title(&names[2], &auto_title(&spawn)));
        assert!(!adopt_spawn_title(&names[2], &names[3]), "чужое имя не своё");

        // Эффект -> нажать: имя эффекта переписывается, набранное руками - нет.
        assert!(adopt_spawn_title(&names[3], &auto_title(&effect)));
        assert!(!adopt_spawn_title("Мой заголовок", &names[3]));

        // Нажать -> держать той же клавишей: тоже своё имя, тоже переписываем.
        assert!(adopt_spawn_title(&names[0], &auto_title(&press)));
    }

    #[test]
    fn columns_follow_width() {
        assert_eq!(column_count(500.0, 1.0, 10.0), 1);
        assert_eq!(column_count(700.0, 1.0, 10.0), 2);
        assert_eq!(column_count(1100.0, 1.0, 10.0), 3);
        assert_eq!(column_count(1100.0, 2.0, 20.0), 1);
        assert_eq!(column_count(40.0, 1.0, 10.0), 1);
    }

    /// Анимации не должны зависеть от частоты кадров: у игрока она гуляет от
    /// 30 до 144, и линейный шаг «на кадр» дал бы на разных машинах разную
    /// скорость. Отсюда экспонента в `approach`.
    #[test]
    fn animation_speed_does_not_depend_on_frame_rate() {
        let run = |frames: u32, dt: f32| {
            let mut v = 0.0;
            for _ in 0..frames {
                v = approach(v, 1.0, dt, 0.1);
            }
            v
        };
        let at_60 = run(60, 1.0 / 60.0);
        let at_30 = run(30, 1.0 / 30.0);
        let at_144 = run(144, 1.0 / 144.0);
        assert!((at_60 - at_30).abs() < 0.01, "60 fps: {at_60}, 30 fps: {at_30}");
        assert!((at_60 - at_144).abs() < 0.01, "60 fps: {at_60}, 144 fps: {at_144}");
        assert!(at_60 > 0.99, "за секунду переход обязан закончиться: {at_60}");
    }

    /// Всё окно прогоняется настоящим ImGui: каждый раздел, поиск и три
    /// разрешения. Игру Claude не запускает, а это ловит ровно то, из-за чего
    /// она падала бы или рисовала кашу: несведённый стек стилей, лишний
    /// `unindent`, обращение к шрифту не из атласа.
    #[test]
    fn every_page_draws_a_real_frame() {
        for size in [[1920.0, 1080.0], [2560.0, 1440.0], [3840.0, 2160.0]] {
            let mut ctx = Context::create();
            // Иначе ImGui на дропе контекста кладёт imgui.ini в корень
            // проекта: тест не должен оставлять за собой файлов.
            ctx.set_ini_filename(None);
            let base = Config::default();
            crate::overlay::load_fonts(&mut ctx, &base);
            ctx.fonts().build_rgba32_texture();
            ctx.io_mut().display_size = size;
            ctx.io_mut().delta_time = 1.0 / 60.0;

            // Подпись, которой не хватило места, обязана быть обрезана, а
            // не нарисована поверх соседней плитки - это и была жалоба
            // 2026-08-20. Ширина глифов берётся из настоящего атласа.
            {
                let ui = ctx.frame();
                for text in [
                    "Прозрачность фона",
                    "Показывать никнеймы над врагами",
                    "http://127.0.0.1:5757/toasts",
                    "Именованные, 165 + 42 DLC",
                    "коротко",
                ] {
                    for room in [40.0, 80.0, 150.0, 400.0] {
                        let cut = clip_to(ui, text, room);
                        assert!(ui.calc_text_size(&cut)[0] <= room, "{text:?} при {room}: вышло {cut:?}");
                    }
                }
                ctx.render();
            }

            for page in 0..pages().len() {
                let mut c = base.clone();
                let mut open = true;
                let mut drag = None;
                let mut capture = None;
                let mut rewards = vec![
                    RewardEntry {
                        id: 1,
                        enabled: true,
                        action: Action::Press { key: hudhook::imgui::Key::Space },
                        cost: 100,
                        reward_id: String::new(),
                        reward_title: "прыжок".into(),
                        label: String::new(),
                        synced: 0,
                    },
                    RewardEntry {
                        id: 2,
                        enabled: true,
                        action: Action::SpawnEnemy {
                            key: crate::spawn::SPAWN_TABLE[0].key,
                            delay_secs: 3,
                            cooldown_secs: 60,
                            ally: false,
                            ttl_secs: 0,
                        },
                        cost: 500,
                        reward_id: "abc".into(),
                        reward_title: "медведь".into(),
                        label: String::new(),
                        synced: 0,
                    },
                ];
                let ui = ctx.frame();
                let out = draw(
                    ui,
                    &mut c,
                    &mut open,
                    &mut drag,
                    &Status::Disabled,
                    &[],
                    &mut rewards,
                    &[],
                    &mut capture,
                    None,
                    0,
                    0,
                    1.0,
                );
                // Кадр без единого клика ничего менять не должен: иначе окно
                // переписывало бы `.ini` само по себе.
                assert!(out.changes.is_empty(), "разрешение {size:?}, раздел {page}");
                ctx.render();

                // Через `draw` виден только тот раздел, который выбран
                // вкладкой (а вкладку из теста не нажать), поэтому плитки
                // каждого раздела гоняем напрямую - ради них тест и написан.
                //
                // Одна награда раскрыта: свёрнутая строка это три виджета, а
                // весь новый код карточки (свои цвета кнопки, выравнивание
                // текста, разделители, панель действий) живёт в раскрытой.
                // Несведённый стек стилей там ассертит уже в ImGui.
                *OPEN_REWARD.lock().unwrap_or_else(|e| e.into_inner()) = Some(2);
                // Свёрнутые блоки тоже разворачиваем: под ними половина
                // виджетов раздела, и в свёрнутом виде тест их не увидит.
                GUIDE_OPEN.store(true, Ordering::Relaxed);
                TOAST_OPEN.store(true, Ordering::Relaxed);
                let ui = ctx.frame();
                let mut c = base.clone();
                // Непустой ЧС: у него свой стек цветов вокруг чипов, и
                // несведённым он утёк бы на всё, что рисуется дальше.
                c.viewer_block = vec!["troll".to_string()];
                let mut drag = None;
                let mut capture = None;
                let mut x = Ctx {
                    k: scale(ui),
                    c: &mut c,
                    changes: Vec::new(),
                    rewards: &mut rewards,
                    drag: &mut drag,
                    capture: &mut capture,
                    status: &Status::Disabled,
                    log: &[],
                    notice: None,
                    connected: true,
                    viewers: &["alpha".to_string(), "beta".to_string()],
                    spawned: 2,
                    effects_on: 1,
                    reset: false,
                    test_purchase: false,
                    test_reward: None,
                    rewards_dirty: false,
                    create_reward: None,
                    delete_on_twitch: None,
                    forget_token: false,
                    clear_spawns: false,
                    audit_spawns: false,
                    clear_effects: false,
                    preview_toast: false,
                    preview_ally: false,
                    preview_fight: false,
                    preview_tag: false,
                    preview_boss: false,
                    refresh_viewers: false,
                    unblocked: None,
                    enable_all_rewards: None,
                };
                ui.window("##probe").size([760.0, 620.0], Condition::Always).build(|| {
                    let mut g = Grid::new(ui, x.k, [0.9, 0.8, 0.5, 1.0], String::new(), page);
                    page_panel(ui, &mut g, &mut x);
                    page_obs(ui, &mut g, &mut x);
                    page_twitch(ui, &mut g, &mut x);
                    page_rewards(ui, &mut g, &mut x);
                    page_other(ui, &mut g, &mut x);
                    g.finish(ui);
                    assert!(g.matched > 0, "раздел {page} не дал ни одной плитки");
                });
                assert!(x.changes.is_empty(), "раздел {page} менял настройки сам по себе");
                ctx.render();
            }

            // Окно редактора награды (`reward_editor_window`, запрос
            // 2026-08-24): переключение `OPEN_REWARD` на другую награду
            // обязано перерисовать то же окно другим содержимым, не падая на
            // несведённом стеке стилей, а пропавшая награда - тихо сбросить
            // выбор, а не держать окно на неё вечно.
            {
                let mut rewards = vec![
                    RewardEntry {
                        id: 1,
                        enabled: true,
                        action: Action::Press { key: hudhook::imgui::Key::Space },
                        cost: 100,
                        reward_id: String::new(),
                        reward_title: "прыжок".into(),
                        label: String::new(),
                        synced: 0,
                    },
                    RewardEntry {
                        id: 2,
                        enabled: true,
                        action: Action::Effect { key: "heal", secs: 0, in_boss: true, cooldown_secs: 0 },
                        cost: 250,
                        reward_id: "xyz".into(),
                        reward_title: "лечение".into(),
                        label: String::new(),
                        synced: 0,
                    },
                ];
                let mut c = base.clone();
                let mut open = true;
                let mut drag = None;
                let mut capture = None;
                for id in [1u32, 2, 999] {
                    *OPEN_REWARD.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
                    let ui = ctx.frame();
                    let out = draw(
                        ui, &mut c, &mut open, &mut drag, &Status::Disabled, &[], &mut rewards,
                        &[], &mut capture, None, 0, 0, 1.0,
                    );
                    assert!(out.changes.is_empty(), "окно редактора для id {id} меняло настройки само");
                    ctx.render();
                }
                // 999 в списке наград нет - окно обязано закрыть себя само.
                assert_eq!(
                    *OPEN_REWARD.lock().unwrap_or_else(|e| e.into_inner()),
                    None,
                    "окно редактора не сбросило выбор пропавшей награды"
                );
            }

            // Окно списка боссов - тем же прогоном: несведённый стек стилей
            // или лишний `unindent` в нём уронил бы игру, а не тест.
            {
                let mut c = base.clone();
                let mut open = true;
                let mut drag = None;
                let mut capture = None;
                let mut rewards = Vec::new();
                BOSS_LIST_OPEN.store(true, Ordering::Relaxed);
                for here in [false, true] {
                    BOSS_LIST_HERE.store(here, Ordering::Relaxed);
                    let ui = ctx.frame();
                    let out = draw(
                        ui, &mut c, &mut open, &mut drag, &Status::Disabled, &[], &mut rewards,
                        &[], &mut capture, None, 0, 0, 1.0,
                    );
                    assert!(out.changes.is_empty(), "окно списка боссов меняло настройки само");
                    ctx.render();
                }
                BOSS_LIST_OPEN.store(false, Ordering::Relaxed);
            }

            // И режим поиска: там плитки всех разделов рисуются сразу.
            SEARCH.lock().unwrap().push_str("цвет");
            let mut c = base.clone();
            let mut open = true;
            let mut drag = None;
            let mut capture = None;
            let mut rewards = Vec::new();
            let ui = ctx.frame();
            let _ = draw(ui, &mut c, &mut open, &mut drag, &Status::Disabled, &[], &mut rewards, &[], &mut capture, None, 0, 0, 1.0);
            ctx.render();
            SEARCH.lock().unwrap().clear();

            // Корпус панели обязан рисоваться до самого края окна. ImGui в
            // `Begin` сужает clip rect на половину `WindowPadding` с боков, и
            // полоса слева (5 px при отступе 24) попадала В СРЕЗАННУЮ ЗОНУ
            // целиком: на экране её не было, а в буфере вершин была - три
            // круга проверок 2026-08-20. Ловится только по clip rect команды.
            {
                let mut c = base.clone();
                c.panel_style = crate::config::PanelStyle::Bar;
                c.panel_x = 120.0;
                c.panel_y = 120.0;
                let snap = crate::stats::Snapshot { valid: true, level: 1, ..Default::default() };
                for _ in 0..2 {
                    let ui = ctx.frame();
                    crate::overlay::draw(ui, &snap, &c);
                    ctx.render();
                }
                let ui = ctx.frame();
                crate::overlay::draw(ui, &snap, &c);
                let data = ctx.render();
                let reaches_edge = data.draw_lists().any(|list| {
                    list.commands().any(|cmd| match cmd {
                        hudhook::imgui::DrawCmd::Elements { cmd_params, .. } => cmd_params.clip_rect[0] <= c.panel_x,
                        _ => false,
                    })
                });
                assert!(reaches_edge, "корпус панели срезается краем окна при {size:?}");
            }

            // Форма корпуса. Жалоба 2026-08-20: «плитка с рамкой имеет рамку
            // вокруг, когда надо было только сверху и снизу» и «золотая рамка
            // прямая, нужно чтобы она радиусом по углам заходила».
            //
            // По вершинам это НЕ ловится: и замкнутая рамка, и две скобы дают
            // вершины только в углах - у прямых участков внутренних точек нет.
            // Поэтому проверяем покрытие: попадает ли точка внутрь хоть одного
            // треугольника корпуса. Отличить корпус от полоски прогресса и
            // счётчика (тот же акцент) можно по прозрачности - у него своя,
            // `border_opacity`; плиту находим по своей, `panel_opacity`.
            {
                crate::overlay::set_alpha(1.0);
                const CHROME_A: f32 = 0.77;
                const PLATE_A: f32 = 0.61;
                // Толще и круглее умолчания намеренно: проверяется ФОРМА, а
                // сглаженная линия в полтора пикселя оставляет сплошного ядра
                // меньше пикселя - пробу пришлось бы ставить по десятым.
                const WIDTH: f32 = 6.0;
                const ROUNDING: f32 = 10.0;
                let chrome_a = (CHROME_A * 255.0).round() as u8;
                let plate_a = (PLATE_A * 255.0).round() as u8;
                let snap = crate::stats::Snapshot { valid: true, level: 1, deaths: 2, ..Default::default() };

                for style in [
                    crate::config::PanelStyle::Frame,
                    crate::config::PanelStyle::Bar,
                    crate::config::PanelStyle::BarRight,
                ] {
                    let mut c = base.clone();
                    c.border_opacity = CHROME_A;
                    c.panel_opacity = PLATE_A;
                    c.border_width = WIDTH;
                    c.panel_rounding = ROUNDING;
                    c.panel_style = style;
                    c.panel_x = 200.0;
                    c.panel_y = 200.0;

                    // Окно авторазмерное: свой размер оно узнаёт со второго
                    // кадра, а до этого корпус не рисуется вовсе.
                    let mut tris: Vec<[[f32; 2]; 3]> = Vec::new();
                    let mut plate: Vec<[f32; 2]> = Vec::new();
                    for _ in 0..3 {
                        let ui = ctx.frame();
                        crate::overlay::draw(ui, &snap, &c);
                        let data = ctx.render();
                        tris.clear();
                        plate.clear();
                        for list in data.draw_lists() {
                            let v = list.vtx_buffer();
                            plate.extend(v.iter().filter(|x| x.col[3] == plate_a).map(|x| x.pos));
                            for t in list.idx_buffer().chunks_exact(3) {
                                let p = [
                                    v[t[0] as usize],
                                    v[t[1] as usize],
                                    v[t[2] as usize],
                                ];
                                if p.iter().all(|x| x.col[3] == chrome_a) {
                                    tris.push([p[0].pos, p[1].pos, p[2].pos]);
                                }
                            }
                        }
                    }
                    assert!(!tris.is_empty(), "корпус {style:?} не нарисован при {size:?}");
                    assert!(!plate.is_empty(), "плита не нарисована при {size:?}");

                    let (x0, y0) = (
                        plate.iter().fold(f32::MAX, |m, p| m.min(p[0])),
                        plate.iter().fold(f32::MAX, |m, p| m.min(p[1])),
                    );
                    let (x1, y1) = (
                        plate.iter().fold(f32::MIN, |m, p| m.max(p[0])),
                        plate.iter().fold(f32::MIN, |m, p| m.max(p[1])),
                    );
                    let covered = |q: [f32; 2]| tris.iter().any(|t| in_triangle(q, *t));

                    // Середина левого края, середина правого и середина
                    // верхнего - вот и вся разница между тремя корпусами.
                    let left_mid = [x0 + 1.0, (y0 + y1) * 0.5];
                    let right_mid = [x1 - 1.0, (y0 + y1) * 0.5];
                    let top_mid = [(x0 + x1) * 0.5, y0 + WIDTH * 0.3];
                    // Точки на дугах верхних углов под 45 градусов: прямая
                    // линия, обрывающаяся до скругления, их не накроет.
                    let r = base.s(ROUNDING);
                    let corner_l = [x0 + r * 0.3, y0 + r * 0.3];
                    let corner_r = [x1 - r * 0.3, y0 + r * 0.3];
                    // Корпус не должен вылезать за плиту ни на пиксель.
                    let (cx0, cx1) = (
                        tris.iter().flatten().fold(f32::MAX, |m, p| m.min(p[0])),
                        tris.iter().flatten().fold(f32::MIN, |m, p| m.max(p[0])),
                    );
                    assert!(cx0 >= x0 - 2.0 && cx1 <= x1 + 2.0, "корпус вылез за плиту ({size:?})");

                    match style {
                        crate::config::PanelStyle::Frame => {
                            assert!(covered(top_mid), "скоба обязана идти поверху ({size:?})");
                            assert!(!covered(left_mid), "у скобы не должно быть боковин ({size:?})");
                            assert!(!covered(right_mid), "у скобы не должно быть боковин ({size:?})");
                            assert!(covered(corner_l), "скоба обязана заходить в угол ({size:?})");
                        }
                        crate::config::PanelStyle::Bar => {
                            assert!(covered(left_mid), "полоса обязана идти во всю высоту ({size:?})");
                            assert!(!covered(top_mid), "полоса не должна идти поверху ({size:?})");
                            assert!(!covered(right_mid), "полоса слева, а не справа ({size:?})");
                            assert!(covered(corner_l), "полоса обязана заходить в угол ({size:?})");
                        }
                        // Зеркальная: ровно то же самое, только у правого края.
                        crate::config::PanelStyle::BarRight => {
                            assert!(covered(right_mid), "полоса обязана идти во всю высоту ({size:?})");
                            assert!(!covered(top_mid), "полоса не должна идти поверху ({size:?})");
                            assert!(!covered(left_mid), "полоса справа, а не слева ({size:?})");
                            assert!(covered(corner_r), "полоса обязана заходить в угол ({size:?})");
                        }
                        crate::config::PanelStyle::Bare => {}
                    }
                }
            }

            // Плитки действительно нарисовались, а не пропущены вместе с
            // дочерним окном: без этого тест зеленел бы на пустом кадре.
            let heights = CARD_H.lock().unwrap();
            assert!(!heights.is_empty(), "ни одной плитки при {size:?}");
            assert!(heights.iter().all(|(_, _, h, _)| *h > 0.0), "плитка нулевой высоты при {size:?}");
        }
    }
}




