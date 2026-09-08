//! Настройки: свой разбор `.ini`, без `toml`/`serde`.
//!
//! Формат простой (`key = value`, `;`/`#` комментарии, `[секции]` чисто
//! косметические), а зависимость тянуть на такое незачем. `impl Default` -
//! единственный источник значений по умолчанию: продублированных констант с
//! теми же числами нигде больше нет.
//!
//! Файл - не единственный путь: почти всё правится окном настроек (F7), а F6
//! перечитывает `.ini` с диска. Значения здесь и в окне - одни и те же поля.

use std::path::{Path, PathBuf};

use hudhook::imgui::Key;
use hudhook::windows::Win32::Foundation::HMODULE;
use hudhook::windows::Win32::System::LibraryLoader::GetModuleFileNameW;

use crate::i18n;

const FILE_NAME: &str = "game_information_counter.ini";

/// Компоновка панели. Варианты выбирались по макету `web/preview.html`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Layout {
    /// Значения крупно, подписи мелко под ними, в две колонки. Панель выходит
    /// вдвое короче списка при том же содержимом.
    A,
    /// Одна горизонтальная лента: не отъедает высоту, кладётся вверх или вниз
    /// экрана. Блок боя появляется второй строкой.
    B,
    /// Список «подпись слева, значение справа» - самая привычная форма.
    D,
}

/// Корпус панели: во что оправлено содержимое. Ключей два - `panel_style` для
/// игры и `web_panel_style` для OBS. Сначала был один общий, но поверх игры и
/// поверх сцены OBS удачным оказывается разное: в игре важнее не мешать, в
/// сцене - держать форму (прямой запрос 2026-08-20).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PanelStyle {
    /// Плитка как в окне настроек: тёмный фон и тонкая рамка по всему
    /// периметру.
    Frame,
    /// Корпуса нет вовсе: только текст со своей тенью. Меньше всего лезет в
    /// глаза, но на светлых локациях читается хуже.
    Bare,
    /// Тёмный фон без рамки, вместо неё вертикальная акцентная полоса слева.
    Bar,
    /// Она же зеркально: полоса справа. Нужна, когда панель стоит у правого
    /// края экрана - там полоса слева смотрит «внутрь» кадра, а не наружу
    /// (прямой запрос 2026-08-20).
    BarRight,
}

/// Откуда брать список зрителей для подписей над врагами.
///
/// Два источника дают разное: Helix отдаёт всех сразу, включая молчащих, но
/// требует прав и модераторства; анонимный чат работает всегда, но показывает
/// только тех, кто пишет. По умолчанию решает мод (`Auto`), но выбрать руками
/// бывает нужно - например когда в подписях хочется видеть только активных.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ViewerSource {
    /// Helix, пока он свежий; иначе чат. Так вело себя всегда.
    Auto,
    /// Только интеграция приложения (`helix/chat/chatters`).
    App,
    /// Только анонимный чат - те, кто писал.
    Chat,
}

/// Какое из двух множеств боссов показывать. Оба считаются всегда - разница
/// только в том, какое попадает в HUD.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BossCount {
    /// Строки `GameAreaParam` с именем для показа. Ближе всего к привычным
    /// "165 + 42 DLC".
    Named,
    /// Все строки с флагом победы, включая мини-боссов и повторки.
    All,
}

#[derive(Clone)]
pub struct Config {
    // вывод
    pub overlay_enabled: bool,
    pub web_enabled: bool,
    pub web_port: u16,
    /// Пауза перед установкой оверлея, мс. Калибровочная ручка, а не
    /// украшение: см. `wait_for_game_ready` в lib.rs.
    pub startup_delay_ms: u32,
    /// Виджет в OBS настраивается отдельно от оверлея: он висит на своей сцене,
    /// поверх своего фона, и удачная там прозрачность почти никогда не совпадает
    /// с удачной поверх игры.
    pub web_opacity: f32,
    /// Яркость золотой скобы сверху и снизу плиты - свой параметр, не общий с
    /// `border_opacity`: раньше окантовка виджета была вообще не связана ни с
    /// каким значением (фиксированный цвет в CSS), из-за чего её было не
    /// поднять из настроек (жалоба 2026-08-18).
    pub web_border_opacity: f32,
    /// Общий множитель поверх кеглей виджета - тот же приём, что `ui_scale`
    /// у оверлея, только независимый: страница в OBS не должна дёргаться от
    /// правки размеров, сделанной для игры.
    pub web_scale: f32,
    pub web_layout: Layout,
    /// Корпус виджета в OBS, свой - как прозрачность и кегли.
    pub web_panel_style: PanelStyle,
    /// Скругление углов плиты виджета, базовые пиксели (умножается на
    /// `web_scale` уже на странице).
    pub web_panel_rounding: f32,
    /// Толщина золотой линии виджета - скобы или полосы, смотря какой корпус.
    pub web_border_width: f32,
    /// Кегли виджета - свои, а не общие с оверлеем. Оверлей настраивают глядя
    /// в игру, виджет - глядя в OBS поверх сцены совсем другого размера, и
    /// одно и то же число там и там на глаз почти никогда не совпадает.
    pub web_label_size: f32,
    pub web_value_size: f32,
    pub web_counter_size: f32,
    pub web_boss_name_size: f32,
    pub web_attempt_size: f32,
    /// Карточка покупки в OBS - свой источник и свой размер: она висит в
    /// углу сцены, а не рядом со статистикой.
    pub web_toast_label_size: f32,
    pub web_toast_value_size: f32,
    /// Ширина карточки, базовые пиксели (умножается на `web_scale`).
    pub web_toast_width: f32,
    /// Корпус карточки покупки в OBS - свой, не общий с виджетом статистики.
    /// Она висит в углу сцены отдельным источником, и рамка ей нужна там, где
    /// виджету не нужна.
    pub web_toast_style: PanelStyle,
    pub web_toast_opacity: f32,
    pub web_toast_border_opacity: f32,
    pub web_toast_rounding: f32,
    pub web_toast_border_width: f32,
    /// Разрядка подписей виджета, пиксели. У ImGui letter-spacing нет вовсе,
    /// поэтому в игре она рисуется вручную - и число одно на оба вывода по
    /// смыслу, но своё по величине, как кегли.
    pub web_tracking: f32,
    /// Зазор между строками виджета, базовые пиксели.
    pub web_line_gap: f32,

    // что показывать
    pub show_level: bool,
    pub show_deaths: bool,
    pub show_bosses: bool,

    /// Ближайший живой босс строкой на панели, пока он в `nearest_radius`.
    pub show_nearest_boss: bool,
    pub show_boss_name: bool,
    /// Двойной босс: показывать имена обоих (каждое своей строкой) или только
    /// первого. По умолчанию один - две строки заметно растят панель, а нужны
    /// не всем.
    pub show_all_boss_names: bool,
    /// Строка боя «СМЕРТЕЙ N» - счётчик попыток минус первый заход, то есть
    /// смерти на этом боссе с нуля. Ключ прежний: он уже лежит в `.ini`.
    pub show_attempts: bool,
    pub show_fight_timer: bool,
    pub show_runes: bool,
    pub show_runes_total: bool,
    pub show_playtime: bool,
    pub show_ng: bool,
    pub show_deathless: bool,
    /// Посещено/всего игровых регионов, в процентах.
    pub show_map_explored: bool,
    /// Смертей на боссах за всю историю персонажа, сумма по всем боям.
    pub show_deaths_on_boss: bool,
    /// Комбинация боссы + время игры: `убито / часы`.
    pub show_boss_kill_rate: bool,
    /// Комбинация смерти + время игры: `смерти / часы`.
    pub show_death_rate: bool,
    /// Комбинация истории попыток + числа убитых боссов: среднее число попыток
    /// на одного убитого босса за всю историю файла статистики.
    pub show_avg_attempts: bool,
    /// Полоска прогресса под счётчиком боссов - отдельно от самого счётчика
    /// (`show_bosses`), чтобы можно было оставить "17/206" и убрать только
    /// полоску.
    pub show_boss_bar: bool,
    pub boss_count: BossCount,
    pub layout: Layout,
    /// Корпус панели в игре.
    pub panel_style: PanelStyle,
    /// Скругление углов плиты, базовые пиксели (умножается на `ui_scale`).
    /// Своё у каждого вывода, как и сам корпус: в игре и в сцене OBS панель
    /// разного размера, и одно число смотрится по-разному.
    pub panel_rounding: f32,
    /// Толщина золотой линии - скобы сверху-снизу или полосы сбоку, смотря
    /// какой корпус. Базовые пиксели, умножается на `ui_scale`.
    pub border_width: f32,

    // панель
    pub panel_x: f32,
    pub panel_y: f32,
    pub panel_opacity: f32,
    /// Яркость золотых скоб сверху и снизу плиты.
    pub border_opacity: f32,
    pub ui_scale: f32,
    pub hide_in_cutscene: bool,
    /// Прятать панель вместе с игровым меню (инвентарь, карта, пауза).
    /// Раньше это было зашито без настройки - см. `StreamHud::render`.
    pub hide_in_menu: bool,
    /// Диагностическое окно стоковыми виджетами, поверх всех проверок.
    pub debug: bool,
    /// Код языка интерфейса - имя файла в `locale` без расширения.
    /// `auto` - тот же язык, на котором говорит сама игра.
    pub lang: String,

    // цвета
    pub accent_color: [f32; 4],
    pub label_color: [f32; 4],
    pub value_color: [f32; 4],

    // шрифты
    pub font_path: String,
    /// Подписи метрик.
    pub label_size: f32,
    /// Значения метрик.
    pub value_size: f32,
    /// Счётчик боссов - самая крупная цифра панели.
    pub counter_size: f32,
    /// Имя текущего босса.
    /// Длиннее скольких символов имя босса переносится на следующую строку.
    /// Ноль - не переносить. По символам, а не по пикселям: см.
    /// `overlay::wrap_name`.
    pub boss_name_wrap: u32,
    pub boss_name_size: f32,
    /// Строка «Попытка N · время». Раньше бралась из `value_size` и меняться
    /// отдельно не могла (отзыв 2026-08-18).
    pub attempt_size: f32,
    /// Карточка покупки: ник с ценой и отсчёт (`toast_label_size`), название
    /// награды (`toast_value_size`). Раньше брались из кеглей панели, и
    /// отдельно карточку было не настроить.
    pub toast_label_size: f32,
    pub toast_value_size: f32,
    /// Ширина карточки, базовые пиксели (умножается на `ui_scale`).
    pub toast_width: f32,
    /// Корпус карточки покупки в игре - свой, не общий с панелью: панель
    /// висит постоянно и не должна мешать, а карточка появляется на секунды
    /// и обязана читаться сразу.
    pub toast_style: PanelStyle,
    pub toast_opacity: f32,
    pub toast_border_opacity: f32,
    pub toast_rounding: f32,
    pub toast_border_width: f32,
    /// Разрядка подписей, пиксели: у ImGui letter-spacing нет, текст с ней
    /// рисуется по глифу (`text_tracked`).
    pub tracking: f32,
    /// Зазор между строками панели, базовые пиксели.
    pub line_gap: f32,

    // twitch
    /// Мастер-выключатель интеграции. По умолчанию выключено: без
    /// зарегистрированного приложения ей всё равно нечего делать.
    pub twitch_enabled: bool,
    /// Client ID приложения с dev.twitch.tv. Не секрет (Device Code Flow
    /// обходится без Client Secret), поэтому спокойно живёт в `.ini`.
    /// Читается один раз при запуске сетевого потока - как `web_port`.
    pub twitch_client_id: String,
    /// Показывать карточку «зритель купил ...» поверх игры.
    pub twitch_notify_hud: bool,
    /// Где стоит верхняя карточка покупки, пиксели экрана. Своя пара, а не
    /// `panel_x/panel_y`: карточки и статистика висят в разных углах, иначе
    /// они наезжали бы друг на друга.
    ///
    /// Дефолт - правый верх на 1080p. При отрисовке позиция прижимается к
    /// экрану, поэтому на меньшем разрешении карточка не уезжает за край.
    pub toast_x: f32,
    pub toast_y: f32,
    /// Сколько покупок держим в очереди. Сверх этого баллы возвращаются: лучше
    /// Завесу «для наград нужен Twitch» показали и её закрыли кнопкой. Раздел
    /// после этого не запирается никогда: подключение проверяет сам мод, а
    /// вид наград настраивают заранее.
    pub rewards_notice_seen: bool,
    /// Канал, чей чат слушаем ради никнеймов. Можно вставить ссылку целиком.
    /// Авторизации не требует вообще - чат Twitch читается анонимно.
    pub twitch_channel: String,
    /// Client ID лежал в `.ini` открытым - значит его вписали руками, и при
    /// загрузке файл надо перезаписать зашифрованным. Рабочий флаг, в `.ini`
    /// не уезжает.
    pub secrets_plain: bool,
    /// Никнеймы зрителей над обычными врагами. Боссов не трогает - у них своя
    /// полоска с именем.
    /// «Смерти от зрителей»: сколько раз стримера довели до смерти за баллы.
    /// Считается по двум признакам сразу - см. `StreamHud::viewer_to_blame`.
    pub show_viewer_kills: bool,
    pub enemy_tags: bool,
    /// Сдвиг подписи от точки тега, в единицах интерфейса игры (1080), а не в
    /// пикселях экрана - иначе значение пришлось бы менять при смене
    /// разрешения. Плюс - ниже, минус - выше.
    ///
    /// Дефолт подобран по замеру живьём: игра рисует имя врага примерно на
    /// столько ниже точки, которую отдаёт в теге.
    ///
    /// Подписывать и боссов тоже. Их имя игра рисует внизу экрана, поэтому
    /// над головой ничего не перекрывается.
    pub enemy_tags_bosses: bool,
    /// Подписывать обычных врагов. Отдельно от `enemy_tags_bosses`: бывает
    /// нужно только над боссами и наоборот.
    pub enemy_tags_mobs: bool,
    /// Доводка подписи босса относительно его головы, в единицах интерфейса
    /// (1920x1080). Само место считает проекция (`enemies::boss_tags`), это
    /// только сдвиг - как `enemy_tag_offset_x` у обычных врагов.
    pub boss_tag_offset_x: f32,
    pub boss_tag_offset_y: f32,
    /// Сдвиг подписи по горизонтали, в единицах интерфейса (1920).
    pub enemy_tag_offset_x: f32,
    pub enemy_tag_height: f32,
    pub enemy_tag_size: f32,
    pub enemy_tag_color: [f32; 4],
    /// «Последнее слово»: под ником показать то, что этот зритель последним
    /// написал в чат. По умолчанию выключено - на экран стримера попадает
    /// чужой текст, и решать это должен он сам.
    pub enemy_tag_say: bool,
    /// Куда её класть относительно ника, в тех же единицах интерфейса
    /// (1920x1080), что и сдвиги самого ника. Ноль - строкой прямо под ним.
    pub enemy_say_offset_x: f32,
    pub enemy_say_offset_y: f32,
    /// Сколько секунд реплика висит над врагом, считая от её первого показа.
    /// Дальше гаснет: висящая вечно фраза читается как часть модели, а не как
    /// то, что зритель сейчас сказал.
    pub enemy_say_secs: f32,
    /// Откуда брать зрителей для подписей.
    pub viewers_source: ViewerSource,
    /// Кого не подписывать никогда, ники в нижнем регистре. В `.ini` лежит
    /// одной строкой через запятую: список короткий и правится мышкой, ради
    /// него отдельный файл заводить незачем.
    pub viewer_block: Vec<String>,
    /// Кого из встроенного списка ботов считать обычным зрителем.
    ///
    /// В `.ini` едут ОТКЛОНЕНИЯ, а не готовый список: иначе боты, добавленные
    /// в новой версии мода, не доехали бы до тех, кто список уже правил.
    pub viewer_bots_off: Vec<String>,
    /// Свои боты сверх встроенного списка.
    pub viewer_bots_extra: Vec<String>,
    /// Сколько секунд висит карточка покупки.
    pub twitch_notify_secs: f32,
    /// Сколько заспавненных врагов может висеть в мире одновременно. Новая
    /// покупка сверх лимита возвращается, а не копится.
    pub spawn_limit: u32,
    pub debug_spawn: bool,

    /// Показывать оверлей союзников: кто прислал, здоровье, остаток жизни.
    pub show_allies: bool,
    /// Положение этого оверлея. Своё, а не общее с панелью или карточками:
    /// это третий источник, и в OBS его двигают отдельно от них.
    pub ally_x: f32,
    pub ally_y: f32,
    /// Как далеко от игрока появляется враг, метры игрового мира. В кольце -
    /// это его радиус, перед взглядом - расстояние по прямой.
    pub spawn_radius_m: f32,
    /// Ставить врага перед взглядом, а не случайно вокруг игрока.
    ///
    /// Кольцо честнее по неожиданности, но половина покупок оказывается за
    /// спиной и уезжает из кадра вместе со стримером: зритель заплатил и не
    /// увидел за что. Направление берётся у камеры, а не у модели персонажа -
    /// «перед взглядом» это про то, куда смотрит зритель.
    pub spawn_in_front: bool,
    /// Разрешить спавн врага, пока идёт бой с боссом. Подкидывать третьего
    /// участника в бою на равных - это чаще про испорченную попытку, чем про
    /// веселье, поэтому решает стример.
    ///
    /// Раньше это был `spawn_block_in_boss` с обратным смыслом (запрет).
    /// Перевёрнуто по запросу 2026-09-08: галочка должна разрешать, а не
    /// запрещать. Старый ключ читается с инверсией, чтобы уже настроенные
    /// файлы не поменяли поведение молча.
    pub spawn_in_boss: bool,
    /// То же самое для призванных союзников. Отдельно от врага: помощь в бою с
    /// боссом и помеха в нём - разные вещи.
    pub ally_spawn_in_boss: bool,

    /// Открыть/закрыть окно настроек.
    pub settings_key: Key,

    /// Открыть/закрыть список неубитых боссов.
    pub boss_list_key: Key,

    /// Что считать «рядом» в списке боссов, в метрах. Тайл открытого мира -
    /// 256 м, так что 500 - это соседний тайл и не дальше.
    pub boss_list_radius: f32,
    /// Дальше этого ближайший босс на панели не показывается. Свой, а не общий
    /// с `boss_list_radius`: на панели это «куда идти прямо сейчас», в списке -
    /// «что осталось в округе», и радиусы у них разные по смыслу.
    pub nearest_radius: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            overlay_enabled: true,
            // Выключен по умолчанию: виджет в OBS нужен не всем, а порт лучше не
            // занимать без спроса.
            web_enabled: false,
            web_port: 7777,
            startup_delay_ms: 10_000,
            web_opacity: 0.65,
            web_border_opacity: 0.85,
            web_scale: 2.3,
            web_layout: Layout::D,
            web_panel_style: PanelStyle::Frame,
            web_panel_rounding: 8.0,
            web_border_width: 1.6,
            web_label_size: 14.0,
            web_value_size: 15.0,
            web_counter_size: 20.0,
            web_boss_name_size: 14.0,
            web_attempt_size: 15.0,
            web_toast_label_size: 14.0,
            web_toast_value_size: 15.0,
            web_toast_width: 340.0,
            web_toast_style: PanelStyle::Frame,
            web_toast_opacity: 0.65,
            web_toast_border_opacity: 0.85,
            web_toast_rounding: 8.0,
            web_toast_border_width: 1.6,
            // Ноль по прямому запросу 2026-08-21: в OBS плотнее читается
            // лучше, чем в игре.
            web_tracking: 0.0,
            web_line_gap: 0.0,

            // По умолчанию — только то, что интересно почти всем стримам;
            // остальное (руны, время игры, NG+, новые комбинированные
            // метрики) включается вручную.
            show_level: true,
            show_deaths: true,
            show_bosses: true,
            show_nearest_boss: true,
            show_boss_name: true,
            show_all_boss_names: false,
            show_attempts: true,
            show_fight_timer: true,
            show_runes: false,
            show_runes_total: false,
            show_playtime: false,
            show_ng: false,
            show_deathless: false,
            show_map_explored: false,
            show_deaths_on_boss: false,
            show_boss_kill_rate: false,
            show_death_rate: false,
            show_avg_attempts: false,
            show_boss_bar: true,
            // Со всеми боссами (мини-боссы, повторки) - прямой запрос.
            boss_count: BossCount::All,
            layout: Layout::D,
            panel_style: PanelStyle::Frame,
            // Со скриншота настроенного пользователем вида (2026-08-24, как
            // и остальные дефолты панели и карточки в этом блоке) - было 8.0.
            panel_rounding: 5.0,
            // Было 1.6.
            border_width: 1.0,

            panel_x: 24.0,
            panel_y: 24.0,
            // Было 0.4.
            panel_opacity: 0.3,
            // Поднято с 0.65 - на светлом фоне игры тонкая скоба почти не
            // читалась (жалоба 2026-08-18, скриншот сравнения игра/OBS).
            // 2026-08-24: со скриншота настроенного вида, было 0.85.
            border_opacity: 0.9,
            ui_scale: 1.0,
            hide_in_cutscene: true,
            hide_in_menu: true,
            debug: false,
            lang: i18n::AUTO.to_string(),

            // Золото Эрдтри на почти-чёрном.
            accent_color: hex("DEB870"),
            label_color: hex("C2C2D1"),
            value_color: hex("F2EBD6"),

            font_path: "C:\\Windows\\Fonts\\pala.ttf".into(),
            // Три уровня, не пять: подписи и имя босса делят один размер (оба
            // рисуются как капслочная подпись), значения и время попытки -
            // другой (оба рисуются как обычный текст). Разные числа тут
            // читались как случайный набор, а не как система (отзыв
            // 2026-08-18). Каждое поле по-прежнему можно раздвинуть отдельно.
            label_size: 16.0,
            value_size: 18.0,
            counter_size: 22.0,
            boss_name_size: 16.0,
            boss_name_wrap: 18,
            attempt_size: 18.0,
            toast_label_size: 18.0,
            toast_value_size: 18.0,
            toast_width: 340.0,
            // Корпус "фон и полоса слева" (было Frame), со скриншота
            // настроенного пользователем вида, 2026-08-24.
            toast_style: PanelStyle::Bar,
            // Было 0.8.
            toast_opacity: 0.3,
            toast_border_opacity: 0.8,
            toast_rounding: 5.0,
            // Было 0.5.
            toast_border_width: 1.0,
            // Со скриншота настроенного пользователем вида, 2026-08-24 -
            // было 1.6.
            tracking: 1.1,
            // Было 5.0.
            line_gap: 4.0,

            twitch_enabled: false,
            twitch_client_id: String::new(),
            twitch_notify_hud: true,
            toast_x: 1520.0,
            toast_y: 24.0,
            rewards_notice_seen: false,
            twitch_channel: String::new(),
            secrets_plain: false,
            show_viewer_kills: false,
            enemy_tags: true,
            enemy_tags_bosses: true,
            enemy_tags_mobs: true,
            boss_tag_offset_x: -70.0,
            boss_tag_offset_y: -20.0,
            // Подобрано живьём 2026-08-19: подпись встаёт слева над полоской
            // HP, не наезжая на неё. Числа в единицах интерфейса (1920x1080).
            enemy_tag_offset_x: -70.0,
            enemy_tag_height: -20.0,
            enemy_tag_size: 18.0,
            enemy_tag_say: true,
            enemy_say_offset_x: 0.0,
            // Реплика встаёт НАД ником: под ним её перекрывает полоска HP.
            enemy_say_offset_y: -35.0,
            enemy_say_secs: 10.0,
            viewers_source: ViewerSource::Auto,
            viewer_block: Vec::new(),
            viewer_bots_off: Vec::new(),
            viewer_bots_extra: Vec::new(),
            enemy_tag_color: hex("E8D7A8"),
            twitch_notify_secs: 10.0,
            spawn_limit: 10,
            debug_spawn: false,
            show_allies: true,
            ally_x: 24.0,
            ally_y: 300.0,
            spawn_radius_m: 5.0,
            spawn_in_front: true,
            spawn_in_boss: false,
            ally_spawn_in_boss: true,

            // F5/F6 часто заняты другими модами - если такой загружен рядом,
            // обе клавиши срабатывали бы в двух окнах сразу.
            settings_key: Key::F7,
            boss_list_key: Key::F8,
            boss_list_radius: 500.0,
            nearest_radius: 200.0,
        }
    }
}

impl Config {
    pub fn load(dll_hmodule: usize) -> Self {
        let mut config = Self::default();
        let Some(path) = config_path(dll_hmodule) else {
            return config;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => config.apply(&text),
            // Шаблон пишем только когда файла действительно нет. Любая другая
            // ошибка (кодировка, права) - дефолты на этот запуск, не трогая то,
            // что лежит на диске: вдруг это файл пользователя, который мы
            // просто не смогли прочитать.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = std::fs::write(&path, TEMPLATE);
            }
            Err(_) => {}
        }
        // Вставленный руками секрет уезжает обратно в файл зашифрованным -
        // ровно один раз, при первой же загрузке после правки.
        if config.secrets_plain {
            Self::save_values(dll_hmodule, &[("twitch_client_id", crate::secret::protect(&config.twitch_client_id))]);
            config.secrets_plain = false;
        }
        // `.ini` хранит БАЗОВЫЕ (немасштабированные) кегли - печём `ui_scale`
        // в них один раз при загрузке, как в elden. Дальше весь код рисует
        // кегли как есть, ничего сверху не домножая.
        config.apply_scale(config.ui_scale);
        // Язык - глобальный: его читают и панель, и веб-выход, и сетевой
        // поток Twitch, а таскать `&Config` во все три незачем. Файлы читаются
        // здесь же: они лежат рядом с DLL, а её handle есть только тут.
        i18n::load(dll_hmodule);
        i18n::apply(&config.lang);
        config
    }

    /// Плоский разбор построчно. Неизвестный ключ и мусорное значение просто
    /// пропускаются - поле остаётся дефолтным.
    fn apply(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') || line.starts_with('[') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            let b = || parse_bool(value);
            let f = || value.parse::<f32>().ok();
            match key {
                "overlay_enabled" => set(&mut self.overlay_enabled, b()),
                "web_enabled" => set(&mut self.web_enabled, b()),
                "web_port" => set(&mut self.web_port, value.parse().ok()),
                "startup_delay_ms" => set(&mut self.startup_delay_ms, value.parse().ok()),
                "web_opacity" => set(&mut self.web_opacity, f()),
                "web_border_opacity" => set(&mut self.web_border_opacity, f()),
                "web_scale" => set(&mut self.web_scale, f()),
                "web_layout" => set(&mut self.web_layout, parse_layout(value)),
                "web_panel_style" => set(&mut self.web_panel_style, parse_panel_style(value)),
                "web_panel_rounding" => set(&mut self.web_panel_rounding, f()),
                "web_border_width" => set(&mut self.web_border_width, f()),
                "web_label_size" => set(&mut self.web_label_size, f()),
                "web_value_size" => set(&mut self.web_value_size, f()),
                "web_counter_size" => set(&mut self.web_counter_size, f()),
                "web_boss_name_size" => set(&mut self.web_boss_name_size, f()),
                "web_attempt_size" => set(&mut self.web_attempt_size, f()),
                "web_toast_label_size" => set(&mut self.web_toast_label_size, f()),
                "web_toast_value_size" => set(&mut self.web_toast_value_size, f()),
                "web_toast_width" => set(&mut self.web_toast_width, f()),
                "web_toast_style" => set(&mut self.web_toast_style, parse_panel_style(value)),
                "web_toast_opacity" => set(&mut self.web_toast_opacity, f()),
                "web_toast_border_opacity" => set(&mut self.web_toast_border_opacity, f()),
                "web_toast_rounding" => set(&mut self.web_toast_rounding, f()),
                "web_toast_border_width" => set(&mut self.web_toast_border_width, f()),
                "web_tracking" => set(&mut self.web_tracking, f()),
                "web_line_gap" => set(&mut self.web_line_gap, f()),

                "show_level" => set(&mut self.show_level, b()),
                "show_deaths" => set(&mut self.show_deaths, b()),
                "show_bosses" => set(&mut self.show_bosses, b()),
                "show_nearest_boss" => set(&mut self.show_nearest_boss, b()),
                "show_boss_name" => set(&mut self.show_boss_name, b()),
                "show_all_boss_names" => set(&mut self.show_all_boss_names, b()),
                "show_attempts" => set(&mut self.show_attempts, b()),
                "show_fight_timer" => set(&mut self.show_fight_timer, b()),
                "show_runes" => set(&mut self.show_runes, b()),
                "show_runes_total" => set(&mut self.show_runes_total, b()),
                "show_playtime" => set(&mut self.show_playtime, b()),
                "show_ng" => set(&mut self.show_ng, b()),
                "show_deathless" => set(&mut self.show_deathless, b()),
                "show_deaths_on_boss" => set(&mut self.show_deaths_on_boss, b()),
                "show_boss_kill_rate" => set(&mut self.show_boss_kill_rate, b()),
                "show_death_rate" => set(&mut self.show_death_rate, b()),
                "show_avg_attempts" => set(&mut self.show_avg_attempts, b()),
                "show_boss_bar" => set(&mut self.show_boss_bar, b()),
                "show_map_explored" => set(&mut self.show_map_explored, b()),
                "layout" => set(&mut self.layout, parse_layout(value)),
                "panel_style" => set(&mut self.panel_style, parse_panel_style(value)),
                "panel_rounding" => set(&mut self.panel_rounding, f()),
                "border_width" => set(&mut self.border_width, f()),
                "boss_count" => {
                    set(
                        &mut self.boss_count,
                        match value.to_ascii_lowercase().as_str() {
                            "named" => Some(BossCount::Named),
                            "all" => Some(BossCount::All),
                            _ => None,
                        },
                    );
                }

                "panel_x" => set(&mut self.panel_x, f()),
                "panel_y" => set(&mut self.panel_y, f()),
                "panel_opacity" => set(&mut self.panel_opacity, f()),
                "border_opacity" => set(&mut self.border_opacity, f()),
                // Клампится сразу: 0 или отрицательное значение из
                // ручной правки .ini иначе делит на ноль в `apply_scale`
                // и при сохранении базового размера кеглей (см. `settings.rs`).
                "ui_scale" => set(&mut self.ui_scale, f().map(|v: f32| v.clamp(0.1, 10.0))),
                "hide_in_cutscene" => set(&mut self.hide_in_cutscene, b()),
                "hide_in_menu" => set(&mut self.hide_in_menu, b()),
                "debug" => set(&mut self.debug, b()),
                "lang" => self.lang = value.to_string(),

                "accent_color" => set(&mut self.accent_color, parse_color(value)),
                "label_color" => set(&mut self.label_color, parse_color(value)),
                "value_color" => set(&mut self.value_color, parse_color(value)),

                "font_path" => self.font_path = value.to_string(),
                "label_size" => set(&mut self.label_size, f()),
                "value_size" => set(&mut self.value_size, f()),
                // `title_size` - прежнее имя `counter_size`. Принимаем оба,
                // чтобы уже написанные .ini не потеряли настройку.
                "counter_size" | "title_size" => set(&mut self.counter_size, f()),
                "boss_name_wrap" => set(&mut self.boss_name_wrap, value.trim().parse().ok()),
                "boss_name_size" => set(&mut self.boss_name_size, f()),
                "attempt_size" => set(&mut self.attempt_size, f()),
                "toast_label_size" => set(&mut self.toast_label_size, f()),
                "toast_value_size" => set(&mut self.toast_value_size, f()),
                "toast_width" => set(&mut self.toast_width, f()),
                "toast_style" => set(&mut self.toast_style, parse_panel_style(value)),
                "toast_opacity" => set(&mut self.toast_opacity, f()),
                "toast_border_opacity" => set(&mut self.toast_border_opacity, f()),
                "toast_rounding" => set(&mut self.toast_rounding, f()),
                "toast_border_width" => set(&mut self.toast_border_width, f()),
                "tracking" => set(&mut self.tracking, f()),
                "line_gap" => set(&mut self.line_gap, f()),

                "twitch_enabled" => set(&mut self.twitch_enabled, b()),
                // Секрет в файле лежит зашифрованным (`enc:`), но вписать его
                // руками можно и открытым - тогда шифруем при загрузке.
                "twitch_client_id" => {
                    self.secrets_plain |= !value.trim().is_empty() && !crate::secret::is_protected(value);
                    self.twitch_client_id = crate::secret::reveal(value);
                }
                "twitch_notify_hud" => set(&mut self.twitch_notify_hud, b()),
                "toast_x" => set(&mut self.toast_x, f()),
                "toast_y" => set(&mut self.toast_y, f()),
                "rewards_notice_seen" => set(&mut self.rewards_notice_seen, b()),
                "twitch_channel" => self.twitch_channel = value.to_string(),
                "show_viewer_kills" => set(&mut self.show_viewer_kills, b()),
                "enemy_tags" => set(&mut self.enemy_tags, b()),
                "enemy_tags_bosses" => set(&mut self.enemy_tags_bosses, b()),
                "enemy_tags_mobs" => set(&mut self.enemy_tags_mobs, b()),
                "boss_tag_offset_x" => set(&mut self.boss_tag_offset_x, f()),
                "boss_tag_offset_y" => set(&mut self.boss_tag_offset_y, f()),
                "enemy_tag_offset_x" => set(&mut self.enemy_tag_offset_x, f()),
                "enemy_tag_height" => set(&mut self.enemy_tag_height, f()),
                "enemy_tag_size" => set(&mut self.enemy_tag_size, f()),
                "enemy_tag_say" => set(&mut self.enemy_tag_say, b()),
                "enemy_say_offset_x" => set(&mut self.enemy_say_offset_x, f()),
                "enemy_say_offset_y" => set(&mut self.enemy_say_offset_y, f()),
                "enemy_say_secs" => set(&mut self.enemy_say_secs, f()),
                "viewers_source" => set(&mut self.viewers_source, parse_viewer_source(value)),
                "viewer_block" => self.viewer_block = parse_nick_list(value),
                "viewer_bots_off" => self.viewer_bots_off = parse_nick_list(value),
                "viewer_bots_extra" => self.viewer_bots_extra = parse_nick_list(value),
                "enemy_tag_color" => set(&mut self.enemy_tag_color, parse_color(value)),
                "twitch_notify_secs" => set(&mut self.twitch_notify_secs, f()),
                "spawn_limit" => set(&mut self.spawn_limit, value.parse().ok()),
                "debug_spawn" => set(&mut self.debug_spawn, b()),
                "show_allies" => set(&mut self.show_allies, b()),
                "ally_x" => set(&mut self.ally_x, f()),
                "ally_y" => set(&mut self.ally_y, f()),
                "spawn_radius_m" => set(&mut self.spawn_radius_m, f()),
                "spawn_in_front" => set(&mut self.spawn_in_front, b()),
                "spawn_in_boss" => set(&mut self.spawn_in_boss, b()),
                "ally_spawn_in_boss" => set(&mut self.ally_spawn_in_boss, b()),
                // Прежний ключ с обратным смыслом: `true` значило «запрещено».
                "spawn_block_in_boss" => set(&mut self.spawn_in_boss, b().map(|v| !v)),

                "settings_key" => set(&mut self.settings_key, parse_key(value)),
                "boss_list_key" => set(&mut self.boss_list_key, parse_key(value)),
                "nearest_radius" => set(&mut self.nearest_radius, f()),
                "boss_list_radius" => set(&mut self.boss_list_radius, f()),
                _ => {}
            }
        }
    }

    /// Что влияет на атлас шрифтов. Совпало - перестраивать атлас не нужно.
    pub fn font_signature(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}",
            // Диапазон глифов атласа зависит от текста активного перевода,
            // поэтому смена языка обязана его пересобрать.
            crate::i18n::active(),
            self.font_path,
            self.label_size,
            self.value_size,
            self.counter_size,
            self.boss_name_size,
            self.attempt_size,
            self.toast_value_size.max(self.toast_label_size),
            // Кегль подписи врага тоже задаёт атлас (печётся с запасом x2),
            // поэтому его смена обязана пересобрать шрифт.
            self.enemy_tag_size
        )
    }

    /// Масштабирует захардкоженное в вёрстке число пикселей (отступы, рельс,
    /// высота полоски). Сырой литерал в коде панели - это баг: при
    /// `ui_scale != 1` он не поедет вместе с остальным.
    pub fn s(&self, px: f32) -> f32 {
        px * self.ui_scale
    }

    /// Вторая половина `ui_scale` - для кеглей. `s()` покрывает вёрстку
    /// (отступы, рельс), а кегли настраиваются отдельными полями и без этого
    /// не двигались бы вместе с остальным при масштабировании (как в elden).
    /// Умножает кегли на `k` прямо в себе - обратное (перед записью в .ini)
    /// делает вызывающий, здесь только прямое домножение.
    pub fn apply_scale(&mut self, k: f32) {
        if !k.is_finite() || k <= 0.0 || (k - 1.0).abs() < f32::EPSILON {
            return;
        }
        for v in [
            &mut self.label_size,
            &mut self.value_size,
            &mut self.counter_size,
            &mut self.boss_name_size,
            &mut self.attempt_size,
        ] {
            *v *= k;
        }
    }

    /// Все настраиваемые из окна значения — для кнопки «Сбросить всё»: она
    /// возвращает `Config::default()` и должна записать это в файл целиком,
    /// иначе старые строки в `.ini` вернут всё обратно при перезапуске.
    pub fn all_values(&self) -> Vec<(&'static str, String)> {
        vec![
            ("show_bosses", self.show_bosses.to_string()),
            ("show_nearest_boss", self.show_nearest_boss.to_string()),
            ("show_deaths", self.show_deaths.to_string()),
            ("show_level", self.show_level.to_string()),
            ("show_runes", self.show_runes.to_string()),
            ("show_runes_total", self.show_runes_total.to_string()),
            ("show_playtime", self.show_playtime.to_string()),
            ("show_ng", self.show_ng.to_string()),
            ("show_deathless", self.show_deathless.to_string()),
            ("show_deaths_on_boss", self.show_deaths_on_boss.to_string()),
            ("show_boss_kill_rate", self.show_boss_kill_rate.to_string()),
            ("show_death_rate", self.show_death_rate.to_string()),
            ("show_avg_attempts", self.show_avg_attempts.to_string()),
            ("show_boss_bar", self.show_boss_bar.to_string()),
            ("show_boss_name", self.show_boss_name.to_string()),
            ("show_all_boss_names", self.show_all_boss_names.to_string()),
            ("show_attempts", self.show_attempts.to_string()),
            ("show_fight_timer", self.show_fight_timer.to_string()),
            ("show_map_explored", self.show_map_explored.to_string()),
            ("layout", self.layout.as_key().to_string()),
            ("panel_style", self.panel_style.as_key().to_string()),
            ("panel_rounding", self.panel_rounding.to_string()),
            ("border_width", self.border_width.to_string()),
            ("web_panel_style", self.web_panel_style.as_key().to_string()),
            ("web_panel_rounding", self.web_panel_rounding.to_string()),
            ("web_border_width", self.web_border_width.to_string()),
            ("web_layout", self.web_layout.as_key().to_string()),
            ("web_opacity", self.web_opacity.to_string()),
            ("web_border_opacity", self.web_border_opacity.to_string()),
            ("web_scale", self.web_scale.to_string()),
            ("web_label_size", self.web_label_size.to_string()),
            ("web_value_size", self.web_value_size.to_string()),
            ("web_counter_size", self.web_counter_size.to_string()),
            ("web_boss_name_size", self.web_boss_name_size.to_string()),
            ("web_attempt_size", self.web_attempt_size.to_string()),
            ("web_toast_label_size", self.web_toast_label_size.to_string()),
            ("web_toast_value_size", self.web_toast_value_size.to_string()),
            ("web_toast_width", self.web_toast_width.to_string()),
            ("web_toast_style", self.web_toast_style.as_key().to_string()),
            ("web_toast_opacity", self.web_toast_opacity.to_string()),
            ("web_toast_border_opacity", self.web_toast_border_opacity.to_string()),
            ("web_toast_rounding", self.web_toast_rounding.to_string()),
            ("web_toast_border_width", self.web_toast_border_width.to_string()),
            ("web_tracking", self.web_tracking.to_string()),
            ("web_line_gap", self.web_line_gap.to_string()),
            ("boss_count", match self.boss_count {
                BossCount::Named => "named".into(),
                BossCount::All => "all".to_string(),
            }),
            ("panel_x", self.panel_x.to_string()),
            ("panel_y", self.panel_y.to_string()),
            ("ui_scale", self.ui_scale.to_string()),
            ("panel_opacity", self.panel_opacity.to_string()),
            ("border_opacity", self.border_opacity.to_string()),
            ("label_size", self.label_size.to_string()),
            ("value_size", self.value_size.to_string()),
            ("counter_size", self.counter_size.to_string()),
            ("boss_name_size", self.boss_name_size.to_string()),
            ("boss_name_wrap", self.boss_name_wrap.to_string()),
            ("attempt_size", self.attempt_size.to_string()),
            ("toast_label_size", self.toast_label_size.to_string()),
            ("toast_value_size", self.toast_value_size.to_string()),
            ("toast_width", self.toast_width.to_string()),
            ("toast_style", self.toast_style.as_key().to_string()),
            ("toast_opacity", self.toast_opacity.to_string()),
            ("toast_border_opacity", self.toast_border_opacity.to_string()),
            ("toast_rounding", self.toast_rounding.to_string()),
            ("toast_border_width", self.toast_border_width.to_string()),
            ("tracking", self.tracking.to_string()),
            ("line_gap", self.line_gap.to_string()),
            ("accent_color", color_to_hex(self.accent_color)),
            ("label_color", color_to_hex(self.label_color)),
            ("value_color", color_to_hex(self.value_color)),
            ("overlay_enabled", self.overlay_enabled.to_string()),
            ("hide_in_cutscene", self.hide_in_cutscene.to_string()),
            ("hide_in_menu", self.hide_in_menu.to_string()),
            ("debug", self.debug.to_string()),
            ("lang", self.lang.clone()),
            ("twitch_enabled", self.twitch_enabled.to_string()),
            ("twitch_client_id", crate::secret::protect(&self.twitch_client_id)),
            ("twitch_notify_hud", self.twitch_notify_hud.to_string()),
            ("toast_x", self.toast_x.to_string()),
            ("toast_y", self.toast_y.to_string()),
            ("rewards_notice_seen", self.rewards_notice_seen.to_string()),
            ("twitch_channel", self.twitch_channel.clone()),
            ("show_viewer_kills", self.show_viewer_kills.to_string()),
            ("enemy_tags", self.enemy_tags.to_string()),
            ("enemy_tags_bosses", self.enemy_tags_bosses.to_string()),
            ("enemy_tags_mobs", self.enemy_tags_mobs.to_string()),
            ("boss_tag_offset_x", self.boss_tag_offset_x.to_string()),
            ("boss_tag_offset_y", self.boss_tag_offset_y.to_string()),
            ("enemy_tag_offset_x", self.enemy_tag_offset_x.to_string()),
            ("enemy_tag_height", self.enemy_tag_height.to_string()),
            ("enemy_tag_size", self.enemy_tag_size.to_string()),
            ("enemy_tag_say", self.enemy_tag_say.to_string()),
            ("enemy_say_offset_x", self.enemy_say_offset_x.to_string()),
            ("enemy_say_offset_y", self.enemy_say_offset_y.to_string()),
            ("enemy_say_secs", self.enemy_say_secs.to_string()),
            ("viewers_source", self.viewers_source.as_key().to_string()),
            ("viewer_block", self.viewer_block.join(",")),
            ("viewer_bots_off", self.viewer_bots_off.join(",")),
            ("viewer_bots_extra", self.viewer_bots_extra.join(",")),
            ("enemy_tag_color", color_to_hex(self.enemy_tag_color)),
            ("twitch_notify_secs", self.twitch_notify_secs.to_string()),
            ("spawn_limit", self.spawn_limit.to_string()),
            ("debug_spawn", self.debug_spawn.to_string()),
            ("show_allies", self.show_allies.to_string()),
            ("ally_x", self.ally_x.to_string()),
            ("ally_y", self.ally_y.to_string()),
            ("spawn_radius_m", self.spawn_radius_m.to_string()),
            ("spawn_in_front", self.spawn_in_front.to_string()),
            ("spawn_in_boss", self.spawn_in_boss.to_string()),
            ("ally_spawn_in_boss", self.ally_spawn_in_boss.to_string()),
            ("boss_list_radius", self.boss_list_radius.to_string()),
            ("nearest_radius", self.nearest_radius.to_string()),
        ]
    }

    /// Переписывает конкретные строки `key = value` в `.ini` на месте, не
    /// трогая ни комментарии, ни другие ключи, ни их порядок, ни переводы
    /// строк. Так окно настроек сохраняет изменения без полного сериализатора
    /// `Config -> ini`, который пришлось бы держать в согласии с `TEMPLATE` и
    /// `apply` — то есть перечислять все ключи в третий раз.
    ///
    /// Молча ничего не делает, если файл не читается или не пишется — та же
    /// позиция, что и в `load`: не трогаем то, чего не понимаем.
    pub fn save_values(dll_hmodule: usize, pairs: &[(&str, String)]) {
        let Some(path) = config_path(dll_hmodule) else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        write_atomic(&path, &rewrite_values(&text, pairs));
    }
}

/// Чистое ядро `save_values`, отдельно — чтобы его можно было проверить без
/// настоящего файла и хендла DLL.
fn rewrite_values(text: &str, pairs: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut seen: Vec<&str> = Vec::new();
    for line in text.split_inclusive('\n') {
        let (body, nl) = line.strip_suffix('\n').map_or((line, ""), |b| (b, "\n"));
        let (body, cr) = body.strip_suffix('\r').map_or((body, ""), |b| (b, "\r"));
        let trimmed = body.trim_start();
        let is_comment = trimmed.starts_with(';') || trimmed.starts_with('#');
        let key = (!is_comment)
            .then(|| trimmed.split_once('='))
            .flatten()
            .map(|(k, _)| k.trim());
        let matched = key.and_then(|k| pairs.iter().find(|(pk, _)| *pk == k));

        match matched {
            Some((k, v)) => {
                seen.push(k);
                out.push_str(&format!("{k} = {v}{cr}{nl}"));
            }
            None => out.push_str(line),
        }
    }

    // Ключ, которого в файле ещё нет, дописываем в конец. Раньше такие просто
    // терялись, и настройка молча откатывалась при следующей перезагрузке.
    let mut missing = pairs.iter().filter(|(k, _)| !seen.contains(k)).peekable();
    if missing.peek().is_some() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        // Заголовок пишем только один раз за всю жизнь файла: секции для
        // `apply` косметические, а два одинаковых блока подряд выглядят мусором.
        if !text.lines().any(|l| l.trim() == "[added]") {
            out.push_str(
                "\n[added]\n\
                 ; Settings changed in game that had no line in this file yet.\n",
            );
        }
        for (k, v) in missing {
            out.push_str(&format!("{k} = {v}
"));
        }
    }
    out
}

/// Обратное к `parse_color`, чтобы правка цвета записалась в том же виде, в
/// каком читается.
pub fn color_to_hex(c: [f32; 4]) -> String {
    let ch = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("{:02X}{:02X}{:02X}{:02X}", ch(c[0]), ch(c[1]), ch(c[2]), ch(c[3]))
}

fn set<T>(field: &mut T, parsed: Option<T>) {
    if let Some(v) = parsed {
        *field = v;
    }
}

fn parse_layout(s: &str) -> Option<Layout> {
    match s.trim().to_ascii_lowercase().as_str() {
        "a" => Some(Layout::A),
        "b" => Some(Layout::B),
        "d" => Some(Layout::D),
        _ => None,
    }
}

fn parse_viewer_source(s: &str) -> Option<ViewerSource> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(ViewerSource::Auto),
        "app" | "helix" => Some(ViewerSource::App),
        "chat" | "link" => Some(ViewerSource::Chat),
        _ => None,
    }
}

impl ViewerSource {
    pub fn as_key(self) -> &'static str {
        match self {
            ViewerSource::Auto => "auto",
            ViewerSource::App => "app",
            ViewerSource::Chat => "chat",
        }
    }
}

/// Список ников через запятую. Регистр складываем сразу: Twitch логины
/// нечувствительны к нему, а сравнивать потом пришлось бы на каждый кадр.
fn parse_nick_list(s: &str) -> Vec<String> {
    s.split(',').map(|n| n.trim().to_lowercase()).filter(|n| !n.is_empty()).collect()
}

fn parse_panel_style(s: &str) -> Option<PanelStyle> {
    match s.trim().to_ascii_lowercase().as_str() {
        "frame" => Some(PanelStyle::Frame),
        "bare" | "none" => Some(PanelStyle::Bare),
        "bar" | "bar_left" => Some(PanelStyle::Bar),
        "bar_right" | "barright" => Some(PanelStyle::BarRight),
        _ => None,
    }
}

impl PanelStyle {
    pub fn as_key(self) -> &'static str {
        match self {
            PanelStyle::Frame => "frame",
            PanelStyle::Bare => "bare",
            PanelStyle::Bar => "bar",
            PanelStyle::BarRight => "bar_right",
        }
    }
}

impl Layout {
    pub fn as_key(self) -> &'static str {
        match self {
            Layout::A => "a",
            Layout::B => "b",
            Layout::D => "d",
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `RRGGBB`/`RRGGBBAA` с необязательной `#`. Через `str::get`, а не индексацию,
/// чтобы кривая строка вернула `None`, а не паниковала (`panic = "abort"` в
/// релизе убивает игру).
fn parse_color(s: &str) -> Option<[f32; 4]> {
    let s = s.trim().trim_start_matches('#');
    let channel = |i: usize| -> Option<u8> { u8::from_str_radix(s.get(i..i + 2)?, 16).ok() };
    let r = channel(0)?;
    let g = channel(2)?;
    let b = channel(4)?;
    let a = if s.len() >= 8 { channel(6)? } else { 255 };
    Some([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a as f32 / 255.0])
}

/// `parse_color` для литералов в `Default` - паникует на кривом входе, а туда
/// его может занести только опечатка прямо здесь.
fn hex(s: &str) -> [f32; 4] {
    parse_color(s).unwrap_or_else(|| panic!("invalid built-in hex color literal: {s}"))
}

/// Совпадение с `Debug`-именем варианта `Key`, а не своя таблица имён: так
/// работают все F1-F12, A-Z, GraveAccent и прочее бесплатно.
pub fn parse_key(name: &str) -> Option<Key> {
    Key::VARIANTS
        .iter()
        .copied()
        .find(|k| format!("{k:?}").eq_ignore_ascii_case(name))
}

fn config_path(dll_hmodule: usize) -> Option<PathBuf> {
    dll_sibling(dll_hmodule, FILE_NAME)
}

/// Путь рядом с нашей DLL. `GetModuleFileNameW(None, ..)` вернул бы путь к exe
/// самой игры, поэтому нужен наш `HMODULE`.
pub fn dll_sibling(dll_hmodule: usize, name: &str) -> Option<PathBuf> {
    let mut buf = [0u16; 1024];
    let len = unsafe { GetModuleFileNameW(Some(HMODULE(dll_hmodule as *mut _)), &mut buf) };
    if len == 0 {
        return None;
    }
    let dll_path = PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
    Some(dll_path.parent()?.join(name))
}

/// Имя временного файла: ПОЛНОЕ имя цели плюс `.tmp`.
///
/// Именно полное, а не смена расширения: `with_extension("tmp")` давал один и
/// тот же `game_information_counter.tmp` и для `.stats`, и для `.twitch`, а пишут их
/// разные потоки - render и сетевой. Гонка на этом клала токен в файл
/// статистики и теряла сами попытки.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Запись файла целиком: сначала во временный, потом rename поверх.
///
/// Игра падает достаточно часто, чтобы обрезанный на середине записи файл был
/// реальным сценарием, а не теоретическим. Один helper на все четыре файла
/// мода - `.ini`, `.stats`, `.rewards`, `.twitch`.
pub fn write_atomic(path: &Path, text: &str) {
    let tmp = tmp_path(path);
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Шаблон, который пишется при первом запуске. Разбирается обратно ровно в
/// `Config::default()` - на это есть тест, и он же не даёт шаблону разъехаться
/// с дефолтами.
const TEMPLATE: &str = r##"; Game Information Counter - a stats HUD for Elden Ring streams.
;
; Edit this file by hand, or press F7 in game: the settings window writes here.
; The interface text lives in the locale folder next to the DLL - drop another
; file in there to add a language.

[output]

; The overlay drawn on top of the game.
overlay_enabled = true

; A web page for an OBS Browser Source, at http://127.0.0.1:<port>/
; Two separate sources: /hud for the stats, /toasts for viewer purchases.
web_enabled = false
web_port = 7777

; Wait before the overlay starts, ms. Raise it if the game hangs on
; startup, lower it if the HUD takes too long to appear.
startup_delay_ms = 10000

; Interface language: auto, or the name of a file in the locale folder.
; auto follows the language the game itself speaks (the Steam per-game setting).
lang = auto

; A window with raw values, drawn even when the HUD is hidden.
; Turn it on if the HUD never shows up.
debug = false

[show]

; The numbers on the panel.
show_level = true
show_deaths = true
show_bosses = true
show_runes = false
show_runes_total = false
show_playtime = false
show_ng = false
show_deathless = false

; Visited play regions, as a percent.
show_map_explored = false

; The progress bar under the boss counter, separately from the counter.
show_boss_bar = true

; The closest boss that is still alive, and how far away he is.
show_nearest_boss = true

; These three appear only while a boss health bar is on screen.
show_boss_name = true
show_attempts = true
show_fight_timer = true

; Both names of a twin boss, one per line, instead of only the first.
show_all_boss_names = false

; Deaths on bosses over the whole history, and how many of those the viewers
; caused (their enemy killed you, or their meddling did).
show_deaths_on_boss = false
show_viewer_kills = false

; Pace: bosses per hour, deaths per hour, deaths per killed boss.
show_boss_kill_rate = false
show_death_rate = false
show_avg_attempts = false

; Which bosses count towards X/Y.
;   named - the named ones only
;   all   - every GameAreaParam row, minibosses and repeats included
; Both numbers come from the game itself, so a mod that adds bosses raises the
; total on its own.
boss_count = all

[panel]

; Panel layout.
;   a - values large, labels under them, two columns
;   b - one horizontal strip
;   d - a list: label on the left, value on the right
layout = d

; Panel frame.
;   frame     - a border all around
;   bare      - no background and no lines, text only
;   bar       - dark fill and an accent bar on the left
;   bar_right - the same bar on the right
panel_style = frame

; Corner radius and gold line width, pixels (both scaled by ui_scale).
panel_rounding = 5
border_width = 1.0

; Where the panel sits, pixels. Drag it in F7 -> Panel.
panel_x = 24
panel_y = 24

; Plate fill and gold line brightness, 0 to 1.
panel_opacity = 0.3
border_opacity = 0.9

; Scales the whole panel: spacing, lines and font sizes alike.
ui_scale = 1.0

; Hide the HUD during cutscenes and while a game menu is open.
hide_in_cutscene = true
hide_in_menu = true

[colors]

; RRGGBB or RRGGBBAA. Shared by the panel and the OBS widget.
accent_color = DEB870
label_color = C2C2D1
value_color = F2EBD6

[fonts]

; Any TTF. Only the glyphs the interface actually needs are baked, so a font
; without Cyrillic (or without CJK) shows those languages as question marks.
font_path = C:\Windows\Fonts\pala.ttf

; Font sizes on the panel, pixels before ui_scale.
label_size = 16
value_size = 18
counter_size = 22
boss_name_size = 16

; Boss names longer than this wrap onto the next line. 0 - never wrap.
boss_name_wrap = 18

; The "deaths / fight time" rows, which have their own size.
attempt_size = 18

; Space between letters in labels and between rows, pixels.
tracking = 1.1
line_gap = 4

[toast]

; The "a viewer bought ..." card over the game.
twitch_notify_hud = true

; Where the card sits, pixels. Drag it in F7 -> Panel.
toast_x = 1520
toast_y = 24

; How long the card stays up, seconds. A timed effect keeps its card for as
; long as it runs.
twitch_notify_secs = 10

; The card has its own frame, sizes and colors - it sits in a corner, away from
; the panel. Values are the same as the panel ones above.
toast_style = bar
toast_rounding = 5
toast_border_width = 1.0
toast_opacity = 0.3
toast_border_opacity = 0.8
toast_label_size = 18
toast_value_size = 18

; The card never grows wider than this, pixels.
toast_width = 340

[obs]

; The widget hangs over an OBS scene, not over the game, so it is configured
; apart from the overlay. The keys mean the same as the ones above.
web_opacity = 0.65
web_border_opacity = 0.85
web_scale = 2.3
web_layout = d
web_panel_style = frame
web_panel_rounding = 8
web_border_width = 1.6
web_label_size = 14
web_value_size = 15
web_counter_size = 20
web_boss_name_size = 14
web_attempt_size = 15
web_tracking = 0
web_line_gap = 0

; The purchase card in OBS: its own source, its own size.
web_toast_style = frame
web_toast_rounding = 8
web_toast_border_width = 1.6
web_toast_opacity = 0.65
web_toast_border_opacity = 0.85
web_toast_label_size = 14
web_toast_value_size = 15
web_toast_width = 340

[twitch]

; Viewers spend channel points and something happens in the game.
;
; How to turn it on:
;   1. dev.twitch.tv/console/apps/create - register an application.
;      Client Type must be Public, OAuth Redirect URL - http://localhost
;   2. Paste the Client ID below. Plain text is fine: on the first run the mod
;      rewrites it encrypted (enc:...) against your Windows account, and such a
;      line is unreadable on any other machine.
;   3. Set twitch_enabled = true. No restart needed.
;   4. In game: F7 -> Twitch -> Connect. A code appears for twitch.tv/activate.
;   5. Create the rewards on the Twitch dashboard, or in F7 -> Rewards.
;
; The token is saved next to this file, encrypted the same way. Do not share
; either file: on your machine they work.
twitch_enabled = false
twitch_client_id =

; The channel whose chat is read for viewer nicknames. Name or a full link.
; No authorization needed for this - Twitch chat is read anonymously.
twitch_channel =

; The "rewards need Twitch" notice was dismissed. Set to 0 to see it again.
rewards_notice_seen = false

; Where the viewer list comes from.
;   auto - the app while it answers, chat otherwise
;   app  - the app only: everyone, lurkers included (needs moderator:read:chatters)
;   chat - chat only: whoever types
viewers_source = auto

; Viewers that never get labeled, comma separated.
viewer_block =

; Chat bots and the streamer never get labeled. The built-in list is edited in
; F7 -> Twitch -> Viewers; only your changes to it live here.
viewer_bots_off =
viewer_bots_extra =

[spawn]

; How many spawned enemies may be in the world at once.
spawn_limit = 10



debug_spawn = false

; Show summoned allies with their health and time left.
show_allies = true
ally_x = 24
ally_y = 300

; How far from the player it appears, meters.
spawn_radius_m = 5

; Put it in front of the camera instead of anywhere around the player.
spawn_in_front = true

; Allow spawning while a boss health bar is on screen.
spawn_in_boss = false
ally_spawn_in_boss = true

; How far the boss list looks for nearby bosses, meters.
boss_list_radius = 500

; How far the nearest boss is still shown on the panel, in meters.
nearest_radius = 200

[nicknames]

; Viewer nicknames over enemies.
enemy_tags = true
enemy_tags_mobs = true
enemy_tags_bosses = true

; Label size and color.
enemy_tag_size = 18
enemy_tag_color = E8D7A8

; Label offset from the enemy health bar. Given in the game's UI units
; (1920x1080) and rescaled to your resolution on its own.
enemy_tag_offset_x = -70
enemy_tag_height = -20

; The same for a boss, whose label is placed above his head.
boss_tag_offset_x = -70
boss_tag_offset_y = -20

; The viewer's last chat message, under their nickname. Off by default:
; this is someone else's text on your stream.
enemy_tag_say = true
enemy_say_offset_x = 0
enemy_say_offset_y = -35

; How long the message stays up, counted from when it first appeared.
enemy_say_secs = 10

[keys]

; The settings window and the boss list.
settings_key = F7
boss_list_key = F8
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// Все четыре файла мода пишутся через временный, и два из них - из разных
    /// потоков (`.stats` из render, `.twitch` из сетевого). Общее имя
    /// временного файла означало бы токен внутри файла статистики, поэтому
    /// суффикс клеится к ПОЛНОМУ имени, а не заменяет расширение.
    #[test]
    fn every_file_gets_its_own_tmp_name() {
        let names = ["game_information_counter.ini", "game_information_counter.stats", "game_information_counter.rewards", "game_information_counter.twitch"];
        let tmps: Vec<PathBuf> = names.iter().map(|n| tmp_path(Path::new(n))).collect();
        for (i, a) in tmps.iter().enumerate() {
            assert_ne!(a, Path::new(names[i]), "временный совпал с целевым: {a:?}");
            for b in &tmps[i + 1..] {
                assert_ne!(a, b, "два файла делят один временный: {a:?}");
            }
        }
    }

    /// Шаблон должен разбираться ровно в дефолты - иначе первый же запуск
    /// молча меняет поведение по сравнению с "конфига нет".
    #[test]
    fn template_round_trips_to_defaults() {
        let mut c = Config::default();
        c.apply(TEMPLATE);
        let d = Config::default();
        assert_eq!(c.overlay_enabled, d.overlay_enabled);
        assert_eq!(c.web_enabled, d.web_enabled);
        assert_eq!(c.web_port, d.web_port);
        assert_eq!(c.startup_delay_ms, d.startup_delay_ms);
        assert_eq!(c.boss_count, d.boss_count);
        assert_eq!(c.layout, d.layout);
        assert_eq!(c.show_runes_total, d.show_runes_total);
        assert_eq!(c.show_deathless, d.show_deathless);
        assert_eq!(c.show_deaths_on_boss, d.show_deaths_on_boss);
        assert_eq!(c.show_boss_kill_rate, d.show_boss_kill_rate);
        assert_eq!(c.show_death_rate, d.show_death_rate);
        assert_eq!(c.show_avg_attempts, d.show_avg_attempts);
        assert_eq!(c.show_boss_bar, d.show_boss_bar);
        assert_eq!(c.show_map_explored, d.show_map_explored);
        assert_eq!(c.hide_in_menu, d.hide_in_menu);
        assert_eq!(c.panel_x, d.panel_x);
        assert_eq!(c.panel_opacity, d.panel_opacity);
        assert_eq!(c.debug, d.debug);
        assert_eq!(c.accent_color, d.accent_color);
        assert_eq!(c.label_color, d.label_color);
        assert_eq!(c.value_color, d.value_color);
        assert_eq!(c.label_size, d.label_size);
        assert_eq!(c.value_size, d.value_size);
        assert_eq!(c.counter_size, d.counter_size);
        assert_eq!(c.boss_name_size, d.boss_name_size);
        assert_eq!(c.attempt_size, d.attempt_size);
        assert_eq!(c.web_attempt_size, d.web_attempt_size);
        assert_eq!(c.web_opacity, d.web_opacity);
        assert_eq!(c.web_layout, d.web_layout);
        assert_eq!(c.web_border_opacity, d.web_border_opacity);
        assert_eq!(c.web_label_size, d.web_label_size);
        assert_eq!(c.web_counter_size, d.web_counter_size);
        assert_eq!(c.spawn_limit, d.spawn_limit);
        assert_eq!(c.spawn_limit, d.spawn_limit);
        assert_eq!(c.spawn_radius_m, d.spawn_radius_m);
        assert_eq!(c.spawn_in_front, d.spawn_in_front);
        assert_eq!(c.spawn_in_boss, d.spawn_in_boss);
        assert_eq!(c.settings_key, d.settings_key);
        assert_eq!(c.boss_list_key, d.boss_list_key);
        assert_eq!(c.boss_list_radius, d.boss_list_radius);
        assert_eq!(c.lang, d.lang);
        // Путь в шаблоне экранирован по-ini-шному, разбирается как есть.
        assert!(c.font_path.to_ascii_lowercase().ends_with("pala.ttf"));
    }

    #[test]
    fn junk_values_leave_defaults_alone() {
        let mut c = Config::default();
        c.apply("web_port = не число\npanel_x = \naccent_color = ZZZZZZ\nboss_count = maybe\nunknown_key = 5");
        let d = Config::default();
        assert_eq!(c.web_port, d.web_port);
        assert_eq!(c.panel_x, d.panel_x);
        assert_eq!(c.accent_color, d.accent_color);
        assert_eq!(c.boss_count, d.boss_count);
    }

    #[test]
    fn comments_and_sections_are_skipped() {
        let mut c = Config::default();
        c.apply("; web_port = 1\n# web_port = 2\n[web_port = 3]\nweb_port = 9001");
        assert_eq!(c.web_port, 9001);
    }

    #[test]
    fn parse_color_handles_rgb_rgba_and_junk() {
        assert_eq!(parse_color("FF0000"), Some([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(parse_color("#00FF0080").map(|c| c[3] > 0.4 && c[3] < 0.6), Some(true));
        assert_eq!(parse_color("F"), None);
        assert_eq!(parse_color(""), None);
    }

    #[test]
    fn bools_accept_the_usual_spellings() {
        for s in ["1", "true", "TRUE", "yes", "on"] {
            assert_eq!(parse_bool(s), Some(true), "{s}");
        }
        for s in ["0", "false", "no", "off"] {
            assert_eq!(parse_bool(s), Some(false), "{s}");
        }
        assert_eq!(parse_bool("ага"), None);
    }

    #[test]
    fn rewrite_keeps_everything_it_did_not_touch() {
        let text = "; коммент
[panel]
panel_x = 24
; panel_y = 99
panel_y = 48
";
        let out = rewrite_values(&text, &[("panel_x", "100".into())]);
        assert!(out.contains("panel_x = 100"));
        assert!(out.contains("; коммент"), "{out}");
        assert!(out.contains("[panel]"), "{out}");
        assert!(out.contains("; panel_y = 99"), "закомментированная строка не должна меняться: {out}");
        assert!(out.contains("panel_y = 48"), "{out}");
    }

    #[test]
    fn rewrite_appends_keys_the_file_lacks() {
        let out = rewrite_values("panel_x = 24
", &[("show_deaths", "false".into())]);
        assert!(out.contains("show_deaths = false"), "{out}");
        assert!(out.contains("[added]"), "{out}");
        // Второй проход не должен добавить второй такой же заголовок.
        let out2 = rewrite_values(&out, &[("show_level", "false".into())]);
        assert_eq!(out2.matches("[added]").count(), 1, "{out2}");
    }

    /// Сохранение должно читаться обратно тем же парсером - иначе настройка
    /// молча откатывается при следующем запуске.
    #[test]
    fn saved_values_load_back() {
        let text = rewrite_values(
            TEMPLATE,
            &[
                ("show_deaths", "false".into()),
                ("panel_x", "300".into()),
                ("accent_color", color_to_hex([1.0, 0.0, 0.0, 1.0])),
            ],
        );
        let mut c = Config::default();
        c.apply(&text);
        assert!(!c.show_deaths);
        assert_eq!(c.panel_x, 300.0);
        assert_eq!(c.accent_color, [1.0, 0.0, 0.0, 1.0]);
    }

    /// «Сбросить всё» обязано записать каждое значение, которое умеет менять
    /// окно: пропущенный ключ вернулся бы из старой строки .ini при перезапуске.
    #[test]
    fn reset_writes_every_value_back_to_defaults() {
        let d = Config::default();
        let text = rewrite_values(TEMPLATE, &d.all_values());
        let mut c = Config::default();
        c.show_deaths = false;
        c.panel_x = 999.0;
        // `hide_in_menu` конкретно уже однажды выпадало из `all_values()`
        // молча (поле/парсер/чекбокс были, а в список для "Сбросить всё" не
        // попало) - держать его тут отдельной строкой.
        c.hide_in_menu = false;
        c.apply(&text);
        assert!(c.show_deaths);
        assert_eq!(c.panel_x, d.panel_x);
        assert_eq!(c.accent_color, d.accent_color);
        assert_eq!(c.hide_in_menu, d.hide_in_menu);
    }

    /// Опечатка в корпусе не должна молча менять вид: неизвестное слово
    /// оставляет прежнее значение.
    #[test]
    fn panel_style_parses_every_variant() {
        for (text, want) in [
            ("frame", PanelStyle::Frame),
            ("bare", PanelStyle::Bare),
            ("bar", PanelStyle::Bar),
            ("bar_right", PanelStyle::BarRight),
        ] {
            let mut c = Config::default();
            c.apply(&format!("panel_style = {text}"));
            assert_eq!(c.panel_style, want, "{text}");
        }
        let mut c = Config::default();
        c.panel_style = PanelStyle::Bar;
        c.apply("panel_style = zzz");
        assert_eq!(c.panel_style, PanelStyle::Bar, "мусор не меняет корпус");
    }

    #[test]
    fn layout_parses_all_three() {
        for (text, want) in [("a", Layout::A), ("B", Layout::B), ("d", Layout::D)] {
            let mut c = Config::default();
            c.apply(&format!("layout = {text}"));
            assert_eq!(c.layout, want, "{text}");
        }
        // Неизвестная буква не должна ронять панель в пустоту.
        let mut c = Config::default();
        c.apply("layout = zzz");
        assert_eq!(c.layout, Config::default().layout);
    }

    /// `title_size` переименовали в `counter_size`; старое имя обязано
    /// продолжать работать, иначе у всех, кто уже настроил панель, кегль
    /// молча уедет к значению по умолчанию.
    #[test]
    fn old_title_size_key_still_works() {
        let mut c = Config::default();
        c.apply("title_size = 31");
        assert_eq!(c.counter_size, 31.0);
    }

    /// Настройки виджета не должны цепляться за настройки оверлея.
    #[test]
    fn web_settings_are_independent() {
        let mut c = Config::default();
        c.apply("panel_opacity = 0.9\nweb_opacity = 0.2\nlayout = d\nweb_layout = b");
        assert_eq!(c.panel_opacity, 0.9);
        assert_eq!(c.web_opacity, 0.2);
        assert_eq!(c.layout, Layout::D);
        assert_eq!(c.web_layout, Layout::B);
    }

    /// Кегли виджета не должны цепляться за кегли оверлея - это была прямая
    /// претензия (2026-08-18): в игре и в OBS разные размеры экрана, и
    /// удобный размер для одного почти никогда не совпадает с удобным для
    /// другого.
    #[test]
    fn web_font_sizes_are_independent_of_overlay() {
        let mut c = Config::default();
        c.apply("value_size = 40\nweb_value_size = 15");
        assert_eq!(c.value_size, 40.0);
        assert_eq!(c.web_value_size, 15.0);
    }

    /// Кегль попытки не должен цепляться ни за значения метрик, ни за счётчик
    /// боссов - раньше он их и был, отдельно не настраивался (2026-08-18).
    #[test]
    fn block_list_round_trips_through_the_ini() {
        // Регистр складывается на входе: логины Twitch к нему нечувствительны,
        // а сравнивать потом пришлось бы на каждый кадр.
        let mut c = Config::default();
        c.apply("viewer_block = Alpha, beta ,,GAMMA");
        assert_eq!(c.viewer_block, vec!["alpha", "beta", "gamma"]);
        let saved = c.viewer_block.join(",");
        let mut back = Config::default();
        back.apply(&format!("viewer_block = {saved}"));
        assert_eq!(back.viewer_block, c.viewer_block);
        // Пустая строка - пустой список, а не запись из одного пробела.
        let mut empty = Config::default();
        empty.apply("viewer_block =");
        assert!(empty.viewer_block.is_empty());
    }

    #[test]
    fn attempt_size_is_independent() {
        let mut c = Config::default();
        c.apply("value_size = 40\nattempt_size = 12\nweb_value_size = 9\nweb_attempt_size = 30");
        assert_eq!(c.value_size, 40.0);
        assert_eq!(c.attempt_size, 12.0);
        assert_eq!(c.web_value_size, 9.0);
        assert_eq!(c.web_attempt_size, 30.0);
    }

    /// `ui_scale` должен домножать кегли обратимо - иначе "Масштаб" в
    /// настройках накручивал бы размер шрифта необратимо туда-сюда.
    #[test]
    fn apply_scale_multiplies_font_sizes_and_is_reversible() {
        let base = Config::default();
        let mut c = base.clone();
        c.apply_scale(1.5);
        assert!((c.value_size - base.value_size * 1.5).abs() < 0.01);
        assert!((c.attempt_size - base.attempt_size * 1.5).abs() < 0.01);
        c.apply_scale(1.0 / 1.5);
        assert!((c.value_size - base.value_size).abs() < 0.01);
        assert!((c.label_size - base.label_size).abs() < 0.01);
    }

    #[test]
    fn apply_scale_ignores_degenerate_input() {
        let base = Config::default();
        let mut c = base.clone();
        c.apply_scale(0.0);
        c.apply_scale(-1.0);
        c.apply_scale(f32::NAN);
        c.apply_scale(1.0);
        assert_eq!(c.value_size, base.value_size);
    }

    /// Загрузка запекает `ui_scale` в кегли - только так экраны, где кегль не
    /// пробрасывается на каждый вызов отрисовки, растут вместе с масштабом.
    #[test]
    fn load_bakes_ui_scale_into_font_sizes() {
        let mut c = Config::default();
        c.apply("ui_scale = 2.0\nvalue_size = 18");
        c.apply_scale(c.ui_scale);
        assert!((c.value_size - 36.0).abs() < 0.01);
    }

    /// Мусорный/нулевой `ui_scale` из ручной правки .ini не должен превращать
    /// панель в ничто или в NaN.
    #[test]
    fn ui_scale_from_ini_is_clamped() {
        let mut c = Config::default();
        c.apply("ui_scale = 0");
        assert!(c.ui_scale >= 0.1);
        let mut c2 = Config::default();
        c2.apply("ui_scale = -5");
        assert!(c2.ui_scale >= 0.1);
    }
}
