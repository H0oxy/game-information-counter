//! Всё чтение игровой памяти. Наружу отдаёт один `Snapshot`.
//!
//! Почти всё здесь - типизированные поля крейта `eldenring`, а не сырые
//! оффсеты: во всём моде их ровно два, и оба тут - `IN_CUTSCENE_BYTE` и
//! `PLAYER_CHARACTER_NAME_OFFSET`. Это осознанно: моды, которые держатся на
//! десятке сырых цепочек, молча ломаются на каждом патче игры, а тут
//! ломаться почти нечему.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::collections::{HashMap, HashSet};

use eldenring::cs::{
    BlockId, CSEventFlagMan, CSFeManHudState, CSFeManImp, FieldInsHandle, GameAreaParam, GameDataMan,
    MenuString, PlayRegionParam, PlayerGameData, SoloParamRepository, WorldChrMan,
};
use eldenring::cs::ChrInsExt;
use fromsoftware_shared::FromStatic;

use crate::config::dll_sibling;
use crate::{msg, spawn};

/// Байт "идёт катсцена/загрузка" в блоке флагов. При непрочтении считаем, что
/// катсцена идёт (падаем закрыто - лучше спрятать HUD, чем показать его
/// поверх ролика).
const IN_CUTSCENE_BYTE: usize = 0x113;

/// Как часто пересчитывать убитых боссов. Флагов тысячи, а меняются они раз в
/// несколько минут - делать это каждый кадр незачем.
const BOSS_POLL: Duration = Duration::from_millis(500);

/// Попытки переживают всё и ничем естественным не ограничены, поэтому кап.
/// На экране всё равно только текущий бой.
const MAX_ATTEMPTS: usize = 64;

/// Сколько держать убитого босса на экране после того, как его полоска
/// пропала. Приятно увидеть "убил за 3:41" уже после боя, но висеть до конца
/// сессии он не должен.
const BOSS_LINGER: Duration = Duration::from_secs(20);

/// Накопленная статистика рядом с DLL. Попытки на боссах и стрик без смертей
/// нигде в игре не хранятся - без этого файла они обнулялись бы при каждом
/// перезаходе.
const STATS_FILE: &str = "game_information_counter.stats";

/// Не чаще раза в столько пишем файл. Попытка меняется раз в бой, так что это
/// с запасом; дебаунс тут против кадра, в котором меняется сразу несколько
/// вещей.
/// ponytail: пишем прямо из render-потока - файл меньше килобайта и трогается
/// раз в несколько минут. Уносить в свой поток, если профайлер покажет иное.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(3);

/// `PlayerGameData::character_name` (`[u16; 17]`) в крейте приватное. Смещение
/// выведено двумя сходящимися способами (сумма полей до него и цепочка
/// "PlayerName" из таблицы Hexinton), проверено живьём в elden.
const PLAYER_CHARACTER_NAME_OFFSET: usize = 0x9C;
const PLAYER_CHARACTER_NAME_LEN: usize = 17;

/// Грубая проверка указателя перед разыменованием. Не проверка страницы (для
/// неё нужен SEH, которого в Rust нет) - просто отсев нулей, мелких чисел и
/// `0xFFFF...`, которые реально приходят из недогруженной игры.
fn plausible_ptr(p: usize) -> bool {
    p >= 0x1_0000 && p < 0x7FFF_FFFF_FFFF
}

/// Имя босса с тега, который игра готовит для интерфейса.
///
/// **Своё чтение, а не `MenuString::to_string()` крейта.** Тот идёт по
/// `static_string` до нуля БЕЗ ПРЕДЕЛА, а указатель приезжает из игровой
/// памяти. Живьём 2026-08-28 на игре 2.7.0.0 это вешало игру намертво в первом
/// же кадре с полоской босса: не падение, а многоминутный проход по памяти на
/// игровом потоке - «зависло и имени не показало».
///
/// Здесь длину ограничивает `msg::utf16` (`MAX_CHARS`), а страницы под строкой
/// проверяет `spawn::readable` - оба конца, потому что проверка отвечает про
/// одну страницу, а читать можно через границу. Не прочиталось - `None`, то
/// есть «имени пока нет»: `update_bars` перечитает следующим кадром, это
/// штатное состояние, а не ошибка.
fn tag_name(s: &MenuString) -> Option<String> {
    // Аллоцированная строка знает свою длину, но длина тоже из игровой памяти.
    // 256 - потолок для имени босса с любым запасом.
    if !s.allocated_string.is_empty() && s.allocated_string.len() < 256 {
        if let Ok(text) = s.allocated_string.to_string() {
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    static_name(s.static_string)
}

/// Статическая половина `tag_name` - ровно та, что вешала игру, и потому
/// вынесена отдельно: `MenuString` в тесте не собрать (его `DLString` требует
/// аллокатор игры), а сырой указатель - собрать.
fn static_name(p: *const u16) -> Option<String> {
    let addr = p as usize;
    // `readable` отвечает про одну страницу, а читать можно через границу -
    // отсюда оба конца.
    let end = addr + 2 * msg::MAX_CHARS - 1;
    if !plausible_ptr(addr) || !spawn::readable(addr) || !spawn::readable(end) {
        return None;
    }
    unsafe { msg::utf16(p) }
}

/// Снимок состояния для отрисовки. Копируется целиком, живёт один кадр.
#[derive(Clone, Default)]
pub struct Snapshot {
    /// false = игра ещё не догрузилась, рисовать нечего.
    pub valid: bool,
    pub in_cutscene: bool,
    /// Открыто игровое меню (инвентарь, карта, пауза, отдых у благодати).
    pub menu_open: bool,

    pub deaths: u32,
    pub level: u32,
    pub runes: u32,
    /// Рун собрано за всю жизнь персонажа (`rune_memory`).
    pub runes_total: u32,
    pub play_time_ms: u32,
    pub ng_lvl: u32,

    /// Боссы: (убито, всего). Два режима подсчёта, выбор в конфиге.
    pub bosses_named: (u32, u32),
    pub bosses_all: (u32, u32),

    /// Бой активен по данным самой игры.
    pub boss_fight_active: bool,
    /// Игрок мёртв прямо сейчас. Нужно спавну: отложенная покупка,
    /// дозревающая над трупом, появится не там и не тогда.
    pub player_dead: bool,
    /// Сколько раз зрители довели до смерти. Копится модом: в игре такого нет.
    pub viewer_deaths: u32,
    /// Игрок в Крепости Круглого стола. Там покупки не исполняются вовсе -
    /// это хаб, а не место для драки.
    pub in_hub: bool,

    /// Ближайший живой босс: имя, метры по горизонтали и разница высот.
    /// Заполняется в `lib.rs` из `bosses::nearest` - `Collector` про парамы
    /// локаций не знает.
    pub nearest_boss: Option<(String, f32, f32)>,
    pub boss_name: Option<String>,
    /// Номер текущей попытки на этом боссе.
    pub attempts: u32,
    /// Время текущей (или последней завершённой) попытки, без катсцен.
    pub attempt_secs: f64,
    /// Смертей на боссах за всю историю персонажа: сумма по всем боям из файла
    /// статистики. Известный потолок: считается как "попытки минус первая",
    /// поэтому уход от живого босса и возвращение к нему засчитываются смертью.
    pub deaths_on_boss: u32,

    /// Секунд с последней смерти.
    pub deathless_secs: f64,

    /// Сумма счётчиков попыток по всем боссам в файле статистики - живым и
    /// уже отпущенным (`BOSS_LINGER` их не выкидывает из `attempts`, только
    /// перестаёт обновлять). Комбинируется с числом убитых боссов в
    /// "среднее попыток на босса" (`show_avg_attempts`).
    pub total_attempts: u32,

    /// Посещено/всего игровых регионов (`PlayRegionParam`).
    pub map_explored: (u32, u32),

}

// ---------------------------------------------------------------------------
// Реестр боссов
// ---------------------------------------------------------------------------

/// Одна строка `GameAreaParam`, сведённая к тому, что нам нужно.
struct BossEntry {
    flag: u32,
    /// Есть имя для показа - значит это босс с полоской и титрами, а не
    /// служебная строка. Отделяет привычные "165 боссов" от полного списка,
    /// где ещё и мини-боссы с повторками.
    named: bool,
}

/// Реестр боссов прямо из игры: каждая строка `GameAreaParam` - босс со своим
/// флагом победы. Мод, добавляющий боссов, добавляет строки, и знаменатель
/// растёт сам - ради этого всё и затевалось. Никаких списков флагов в репе.
fn build_registry() -> Option<Vec<BossEntry>> {
    let repo = unsafe { SoloParamRepository::instance() }.ok()?;
    // Один флаг победы - один босс. Вторая строка на тот же флаг описывает
    // вход на большой карте (у Годрика это тайл Лимгрейва рядом с замком), и
    // без дедупликации счётчик панели считал таких дважды: живьём 2026-08-27
    // панель показывала 15/212 против 14/207 в окне списка, которое флаги
    // схлопывает (`bosses::one_per_flag`).
    let mut seen: HashMap<u32, bool> = HashMap::new();
    for (_, r) in repo.rows::<GameAreaParam>().filter(|(_, r)| r.defeat_boss_flag_id() != 0) {
        let named = r.found_boss_text_id() != -1;
        *seen.entry(r.defeat_boss_flag_id()).or_insert(named) |= named;
    }
    let rows: Vec<BossEntry> =
        seen.into_iter().map(|(flag, named)| BossEntry { flag, named }).collect();
    // До загрузки парамов итератор пуст - не запоминаем пустой реестр,
    // попробуем на следующем кадре.
    (!rows.is_empty()).then_some(rows)
}

/// Id всех игровых регионов - знаменатель для "% исследованной карты"
/// (`show_map_explored`). Как и `build_registry`, строится один раз из парама.
///
/// Множество, а не счётчик: `visited_areas` игра пополняет чем угодно, и без
/// пересечения числитель уходил за знаменатель (живьём 2026-08-27 - 279%).
/// Фильтра `disable_param_nt()` тут нет: он отсекает строки, выключенные в
/// сетевом тесте, то есть почти весь контент релиза - именно он и делал
/// знаменатель втрое меньше нужного.
fn build_regions() -> Option<HashSet<u32>> {
    let repo = unsafe { SoloParamRepository::instance() }.ok()?;
    let ids: HashSet<u32> = repo.rows::<PlayRegionParam>().map(|(id, _)| id).collect();
    (!ids.is_empty()).then_some(ids)
}

// ---------------------------------------------------------------------------
// Попытки.
// Логика обкатана живьём, три отдельных бага в ней уже найдены и закрыты - не
// переписывать "как проще", сперва разобраться, что закрывает каждая
// проверка ниже.
// ---------------------------------------------------------------------------

/// Полоска здоровья босса в текущем кадре.
#[derive(Clone, PartialEq)]
struct BossBar {
    handle: FieldInsHandle,
    fmg_id: i32,
    name: Option<String>,
    /// Для какого `fmg_id` было получено закэшированное имя. Босс,
    /// переименовавший себя на второй фазе, сохраняет handle, а нового тега в
    /// `boss_list_tag_data` в тот же кадр ещё нет - старое имя держится
    /// (пустая подпись хуже устаревшей), а это отставшее поле заставляет
    /// перечитывать, пока не появится.
    name_fmg: i32,
    /// Когда начали перечитывать, чтобы было когда закончить: тег, совпадающий
    /// с уже показанным, значит "ещё не обновился" - других сигналов нет, и две
    /// фазы с реально одинаковым именем иначе пересканировали бы список каждый
    /// кадр до конца боя.
    name_pending_since: Option<Instant>,
}

/// Один отслеживаемый бой, ключ - `(fmg_ids, block)`: оба переживают
/// перезагрузку карты, в отличие от целого `FieldInsHandle` (его selector -
/// рантайм-индекс).
struct Attempt {
    /// Все FMG id, под которыми шёл этот бой. Обычно один; Радагон,
    /// превращающийся в Элден Бист, выставляет полоску с другим id посреди боя,
    /// и трактовка этого как нового босса сбрасывала счётчик на 1 и часы на
    /// ноль на середине рана.
    fmg_ids: Vec<i32>,
    /// Блок карты, где живёт босс. Без него все одинаковые боссы, которых игра
    /// переиспользует по миру (Древо-аватары, Древесные духи), делили один
    /// счётчик, и подход к свежему показывал счёт предыдущего. Близнецы в одной
    /// арене блок делят - и правильно, это один бой.
    block: BlockId,
    count: u32,
    /// Часы идут отсюда, пока бой активен.
    started_at: Instant,
    /// Замораживаются в конце попытки: полоска исчезла (убил) или игрок умер.
    /// Появление полоски снова, пока заморожено, начинает новую попытку.
    frozen_at: Option<Instant>,
    /// Заморожено смертью игрока, и полоска с тех пор ни разу не пропадала.
    ///
    /// **Баг, найденный живьём 2026-08-21 на двойном боссе:** при смерти в
    /// соло полоска НЕ исчезает, а `hp` игрока возвращается раньше, чем игра
    /// её убирает (загрузка после «Продолжить»). В это окно попытка
    /// перезапускалась прямо на экране загрузки - плюс один; второй плюс
    /// приходил, когда игрок реально возвращался к боссу. Отсюда правило:
    /// после смерти рестарт ждёт, пока полоска пропадёт хоть на кадр.
    ///
    /// В файл не едет: восстановленная попытка либо заморожена (тогда рестарт
    /// и должен считаться), либо продолжается без заморозки.
    restart_needs_gone: bool,
    /// Сколько раз игрок умер на этом боссе.
    ///
    /// Не то же самое, что `count - 1`: счётчик попыток растёт при
    /// ВОЗВРАЩЕНИИ к боссу, то есть цифра на экране менялась только когда
    /// добежишь до арены заново, а не в момент смерти (жалоба 2026-08-23 -
    /// строку давно переименовали в «СМЕРТЕЙ»). Здесь же растёт по факту
    /// смерти. Побочно уходит и старый потолок: уход от живого босса и
    /// возвращение к нему смертью больше не считается.
    ///
    /// В файл не едет: имя стоит последним полем, и вставить что-то перед ним
    /// нельзя не сломав разбор уже написанных строк. При загрузке берётся
    /// `count - 1` - ровно то, что показывалось раньше.
    deaths: u32,
    /// `death_count` на момент первой встречи с этим боссом.
    deaths_at_start: u32,
    /// Имя босса на момент последнего боя. Нужно только чтобы файл статистики
    /// читался глазами - HUD берёт имя из живой полоски.
    name: Option<String>,
}

impl Attempt {
    /// Этот бой под любым из своих имён.
    fn covers(&self, fmg_id: i32, block: BlockId) -> bool {
        self.block == block && self.fmg_ids.contains(&fmg_id)
    }

    /// Сколько бой в этом блоке остаётся открыт для усыновления нового FMG id
    /// как своей фазы. Хватает, чтобы переименование прилетело с той стороны
    /// переходной катсцены. Известный потолок: два разных босса подряд в одном
    /// блоке внутри окна сольются в один счётчик.
    const PHASE_WINDOW: Duration = Duration::from_secs(60);
}

// ---------------------------------------------------------------------------

pub struct Collector {
    /// Строится один раз: парамы в рантайме не меняются.
    registry: Option<Vec<BossEntry>>,
    /// (убито_named, всего_named, убито_all, всего_all)
    counts: (u32, u32, u32, u32),
    last_poll: Option<Instant>,
    /// Id всех игровых регионов (`PlayRegionParam`) - строится один раз, тем
    /// же таймером, что и `registry`.
    regions: Option<HashSet<u32>>,
    /// Сколько регионов посещено. Пересчитывается по таймеру, а не каждый
    /// кадр: см. `collect`.
    visited_areas: u32,
    last_visited_poll: Option<Instant>,
    /// Когда рефлексия в последний раз НЕ нашла игрока. Только для запасного
    /// пути в `collect`, когда `GameDataMan` недоступен - см. там.
    last_reflection_miss: Option<Instant>,

    bars: Vec<BossBar>,
    /// Значения прошлого кадра - нужны, чтобы заморозить часы уже исчезнувшей
    /// полоски.
    last_bars: Vec<BossBar>,
    /// Когда полоски пропали - для `BOSS_LINGER`.
    bars_cleared_at: Option<Instant>,
    attempts: Vec<Attempt>,
    cutscene_since: Option<Instant>,
    prev_dead: bool,
    /// Полный вайп прошлого кадра. Отдельно от `prev_dead`: в ко-опе моя
    /// смерть часы не останавливает, бой продолжает союзник.
    prev_all_dead: bool,

    /// Смертей по вине зрителей. Живёт в файле статистики: игра такого не
    /// знает, а обнуляться при перезаходе счётчику незачем - ровно тот же
    /// довод, что у попыток.
    viewer_deaths: u32,

    last_deaths: Option<u32>,
    last_death_at: Instant,

    /// Свой `HMODULE` - чтобы найти файл статистики рядом с DLL.
    hmodule: usize,
    /// Имя персонажа, под которым сейчас копится статистика. Смена = сохранить
    /// старого и загрузить нового: у каждого персонажа свои попытки.
    character: Option<String>,
    dirty: bool,
    last_save: Instant,
}

impl Collector {
    pub fn new(hmodule: usize) -> Self {
        Self {
            registry: None,
            counts: (0, 0, 0, 0),
            last_poll: None,
            regions: None,
            visited_areas: 0,
            last_visited_poll: None,
            last_reflection_miss: None,
            bars: Vec::new(),
            last_bars: Vec::new(),
            bars_cleared_at: None,
            attempts: Vec::new(),
            cutscene_since: None,
            prev_dead: false,
            prev_all_dead: false,
            viewer_deaths: 0,
            last_deaths: None,
            last_death_at: Instant::now(),
            hmodule,
            character: None,
            dirty: false,
            last_save: Instant::now(),
        }
    }

    /// Вызывается из render-лупа - это игровой поток, читать память отсюда
    /// безопаснее всего.
    /// Запасной путь к данным игрока, когда `GameDataMan` недоступен.
    ///
    /// Он идёт по RVA, то есть на версии игры, для которой адреса ещё не сняты,
    /// его нет - а он же служил дешёвым признаком «игра готова». Всё остальное
    /// читается через рефлексию и работает, поэтому игрока ищем через
    /// `WorldChrMan`. **При промахе - не чаще раза в полсекунды**: пока игра не
    /// инициализировала рефлексию, каждый промах проходит по всей карте
    /// синглтонов, и в главном меню это роняло FPS до 8 (см. `collect`).
    ///
    /// 0 - «игрока нет»; вызывающий и так проверяет `plausible_ptr`.
    fn player_game_data_by_reflection(&mut self) -> usize {
        if self.last_reflection_miss.is_some_and(|t| t.elapsed() < BOSS_POLL) {
            return 0;
        }
        let found = unsafe { WorldChrMan::instance() }
            .ok()
            .and_then(|w| w.main_player.as_ref())
            .map_or(0, |p| p.player_game_data.as_ptr() as usize);
        self.last_reflection_miss = (found == 0).then(Instant::now);
        found
    }

    pub fn collect(&mut self) -> Snapshot {
        let mut s = Snapshot::default();

        // Порядок здесь - вопрос производительности, а не вкуса.
        // `GameDataMan` резолвится по RVA и стоит копейки, а `CSEventFlagMan`,
        // `CSFeManImp`, `SoloParamRepository` и `WorldChrMan` ищутся через
        // рефлексию Dantelion2: пока игра её не инициализировала, КАЖДЫЙ такой
        // вызов проходит по всей карте синглтонов (`all_null`). В главном меню
        // это роняло игру до ~8 FPS и выглядело как зависание при запуске.
        // Поэтому сначала самый дешёвый признак, и при неудаче - немедленный
        // выход, не трогая больше ничего.
        let gdm = unsafe { GameDataMan::instance() }.ok();
        // В главном меню данных игрока ещё нет. `OwnedPtr` обещает ненулевой
        // указатель, но игра держит там ноль, поэтому читаем сырым и проверяем
        // до разыменования.
        let pgd_ptr = match gdm {
            Some(gdm) => unsafe { *(&gdm.main_player_game_data as *const _ as *const usize) },
            None => self.player_game_data_by_reflection(),
        };
        if !plausible_ptr(pgd_ptr) {
            return s;
        }
        let pgd = unsafe { &*(pgd_ptr as *const PlayerGameData) };

        s.in_cutscene = read_in_cutscene();
        s.menu_open = read_menu_open();
        self.note_cutscene(s.in_cutscene);

        s.valid = true;
        // Всё это живёт только в `GameDataMan`, аналога через рефлексию нет:
        // на версии игры, для которой адреса ещё не сняты, остаётся нулями.
        if let Some(gdm) = gdm {
            s.deaths = gdm.death_count;
            s.play_time_ms = gdm.play_time;
            s.ng_lvl = gdm.ng_lvl;
            s.boss_fight_active = gdm.boss_fight_active;
        }

        s.level = pgd.level;
        s.runes = pgd.rune_count;
        s.runes_total = pgd.rune_memory;

        // `visited_areas` - обычный `Vec` в самой `PlayerGameData`, уже под
        // рукой. Дедуп на случай, если игра не гарантирует уникальность - но
        // не каждый кадр: это аллокация `HashSet` на несколько сотен id ради
        // числа, которое меняется раз в минуты. Тот же таймер, что у боссов.
        if self.last_visited_poll.is_none_or(|t| t.elapsed() >= BOSS_POLL) {
            self.last_visited_poll = Some(Instant::now());
            // Считаем только те id, что реально есть в параме: посторонние
            // числа в `visited_areas` иначе задирают процент выше 100.
            let regions = self.regions.as_ref();
            self.visited_areas = pgd
                .visited_areas
                .iter()
                .copied()
                .filter(|id| regions.is_none_or(|r| r.contains(id)))
                .collect::<HashSet<_>>()
                .len() as u32;
        }
        s.map_explored = (self.visited_areas, self.regions.as_ref().map_or(0, |r| r.len() as u32));

        self.follow_character(read_character_name(pgd));

        // Первый кадр только запоминает счётчик: иначе загрузка персонажа с
        // ненулевыми смертями читалась бы как смерть прямо сейчас.
        match self.last_deaths {
            None => self.last_deaths = Some(s.deaths),
            Some(prev) if prev != s.deaths => {
                self.last_deaths = Some(s.deaths);
                self.last_death_at = Instant::now();
                self.dirty = true;
            }
            _ => {}
        }
        s.deathless_secs = self.last_death_at.elapsed().as_secs_f64();

        let (dead, all_dead, in_hub) = read_player();
        s.player_dead = dead;
        s.viewer_deaths = self.viewer_deaths;
        s.in_hub = in_hub;
        self.update_bars();
        self.update_attempts(s.deaths, dead, all_dead);
        self.fill_current_fight(&mut s);
        self.poll_bosses();

        s.bosses_named = (self.counts.0, self.counts.1);
        s.bosses_all = (self.counts.2, self.counts.3);
        s.total_attempts = self.attempts.iter().map(|a| a.count).sum();
        // Каждая попытка сверх первой начата смертью на этом боссе.
        s.deaths_on_boss = self.attempts.iter().map(|a| a.deaths).sum();

        if self.dirty && self.last_save.elapsed() > SAVE_DEBOUNCE {
            self.save();
        }
        s
    }

    /// Статистика копится на персонажа. Пустое имя (идёт загрузка, игрок ещё
    /// не в мире) сменой не считается - иначе каждый переход карты сбрасывал бы
    /// накопленное.
    fn follow_character(&mut self, name: Option<String>) {
        let Some(name) = name else { return };
        if self.character.as_deref() == Some(name.as_str()) {
            return;
        }
        if self.character.is_some() {
            self.save();
        }
        self.character = Some(name);
        self.load();
    }

    /// Катсцены не считаются временем боя: вся пауза добавляется обратно к
    /// `started_at` **и** `frozen_at`. Двигать оба, а не только идущие, нужно
    /// чтобы показанное время завершённой попытки (разница между ними) стояло
    /// на месте, и чтобы катсцена не съедала `PHASE_WINDOW`, в котором должна
    /// успеть приехать переименованная фаза.
    fn note_cutscene(&mut self, playing: bool) {
        match (playing, self.cutscene_since) {
            (true, None) => self.cutscene_since = Some(Instant::now()),
            (false, Some(since)) => {
                let paused = since.elapsed();
                for a in self.attempts.iter_mut() {
                    a.started_at += paused;
                    if let Some(f) = a.frozen_at.as_mut() {
                        *f += paused;
                    }
                }
                self.cutscene_since = None;
            }
            _ => {}
        }
    }

    /// Полоски боссов текущего кадра, с переносом уже разрешённых имён.
    fn update_bars(&mut self) {
        let Ok(fe) = (unsafe { CSFeManImp::instance() }) else {
            return;
        };
        if !self.bars.is_empty() {
            self.last_bars = self.bars.clone();
        }
        // Имя *забирается* из прошлого кадра, а не перечитывается: это скан
        // `boss_list_tag_data` плюс UTF-16 -> String каждый кадр каждого боя
        // ради строки, которая не меняется.
        let mut previous = std::mem::take(&mut self.bars);
        let mut now_shown = Vec::new();
        for display in fe.boss_health_displays.iter() {
            let handle = display.field_ins_handle;
            if handle.is_empty() {
                continue;
            }
            let (mut name, mut name_fmg, mut pending) = previous
                .iter_mut()
                .find(|b| b.handle == handle)
                .map_or((None, display.fmg_id, None), |b| {
                    (b.name.take(), b.name_fmg, b.name_pending_since)
                });

            if name.is_none() || name_fmg != display.fmg_id {
                /// Сколько даём переименованию доехать, прежде чем тег,
                /// совпадающий с текущим именем, принять за чистую монету.
                const NAME_SETTLE: Duration = Duration::from_secs(5);
                let waited = pending.get_or_insert_with(Instant::now).elapsed();
                let fresh = fe
                    .frontend_values
                    .boss_list_tag_data
                    .iter()
                    .find(|tag| tag.field_ins_handle == handle)
                    .and_then(|tag| tag_name(&tag.chr_name))
                    .filter(|s| !s.is_empty());
                // Отличающееся имя - это доехавшее переименование, берём сразу.
                // Совпадающее - "ещё не обновилось", пока не истёк NAME_SETTLE.
                // Отсутствие тега перечитывает бесконечно: это неподгруженные
                // данные, а не переименование.
                if let Some(fresh) = fresh {
                    if Some(&fresh) != name.as_ref() || waited > NAME_SETTLE {
                        name = Some(fresh);
                        name_fmg = display.fmg_id;
                        pending = None;
                    }
                }
            }
            now_shown.push(BossBar {
                handle,
                fmg_id: display.fmg_id,
                name,
                name_fmg,
                name_pending_since: pending,
            });
        }
        self.bars = now_shown;

        // Полоска пропала: последнего босса держим ещё `BOSS_LINGER`, потом
        // забываем. Заодно это единственное, что не даёт `last_bars` висеть
        // до конца сессии и вечно гонять `freeze_running` вхолостую.
        if self.bars.is_empty() {
            match self.bars_cleared_at {
                None => self.bars_cleared_at = Some(Instant::now()),
                Some(t) if t.elapsed() > BOSS_LINGER => self.last_bars.clear(),
                _ => {}
            }
        } else {
            self.bars_cleared_at = None;
        }
    }

    fn update_attempts(&mut self, deaths: u32, dead: bool, all_dead: bool) {
        let had_bars = !self.last_bars.is_empty();

        // Моя смерть - в счётчик смертей, по фронту. Часы она НЕ трогает: в
        // ко-опе бой продолжает союзник, и попытка идёт дальше.
        if dead && !self.prev_dead {
            self.count_death();
        }
        self.prev_dead = dead;

        // Часы останавливает полный вайп - как `all_players_dead` в elden.
        // В соло это та же самая смерть, только кадром позже некуда.
        if all_dead && !self.prev_all_dead {
            self.freeze_running(true);
        }
        self.prev_all_dead = all_dead;

        // Второй конец попытки - босс умер, и его полоска надёжно пропадает
        // (в отличие от смерти игрока, где она остаётся).
        if had_bars && self.bars.is_empty() {
            self.freeze_running(false);
        }

        // Полоска пропала - ожидание снято: следующее её появление и есть
        // возвращение к боссу. Одного кадра без полоски достаточно.
        for a in &mut self.attempts {
            if a.restart_needs_gone && !self.bars.iter().any(|b| a.covers(b.fmg_id, b.handle.block_id)) {
                a.restart_needs_gone = false;
            }
        }

        let seen: Vec<_> = self
            .bars
            .iter()
            .map(|b| (b.handle, b.fmg_id, b.name.clone()))
            .collect();
        for (handle, fmg_id, name) in seen {
            let block = handle.block_id;
            match self.attempts.iter_mut().find(|a| a.covers(fmg_id, block)) {
                // Id, невиданный в этом блоке. Либо босс, с которым ещё не
                // дрались, либо тот же бой, переименовавшийся на смене фазы -
                // различаем по тому, открыт ли ещё бой в этом блоке.
                None => match self
                    .attempts
                    .iter_mut()
                    .filter(|a| a.block == block)
                    .filter(|a| a.frozen_at.is_none_or(|f| f.elapsed() < Attempt::PHASE_WINDOW))
                    // Самый свежий бой в блоке; живой старше любого
                    // замороженного, за что тут и отвечает `Instant::now()`.
                    .max_by_key(|a| a.frozen_at.unwrap_or_else(Instant::now))
                {
                    // Тот же бой, новое имя: счётчик и часы сохраняются.
                    Some(a) => {
                        a.fmg_ids.push(fmg_id);
                        a.frozen_at = None;
                        a.name = name;
                        self.dirty = true;
                    }
                    None => {
                        self.attempts.push(Attempt {
                            fmg_ids: vec![fmg_id],
                            block,
                            count: 1,
                            started_at: Instant::now(),
                            frozen_at: None,
                            restart_needs_gone: false,
                            deaths: 0,
                            deaths_at_start: deaths,
                            name,
                        });
                        self.dirty = true;
                    }
                },
                // `!all_dead` важно, а не только `frozen_at.is_some()`: при смерти
                // в соло полоска надёжно НЕ исчезает, так что в тот самый кадр,
                // когда заморозка сработала, босс всё ещё в списке - без этой
                // проверки это читалось как "полоска вернулась" и попытка
                // начиналась заново ещё до респавна.
                Some(a) if a.frozen_at.is_some() && !all_dead && !a.restart_needs_gone => {
                    a.count += 1;
                    a.started_at = Instant::now();
                    a.frozen_at = None;
                    if name.is_some() {
                        a.name = name;
                    }
                    self.dirty = true;
                }
                Some(_) => {}
            }
        }

        let excess = self.attempts.len().saturating_sub(MAX_ATTEMPTS);
        if excess > 0 {
            self.attempts.drain(0..excess);
        }
    }

    /// Заморозить часы каждой идущей попытки, чью полоску мы видели в прошлом
    /// кадре. `last_bars`, а не `bars`: к моменту вызова полоска убитого босса
    /// уже пропала.
    fn freeze_running(&mut self, by_death: bool) {
        let now = Instant::now();
        for bar in &self.last_bars {
            if let Some(a) = self
                .attempts
                .iter_mut()
                .find(|a| a.covers(bar.fmg_id, bar.handle.block_id) && a.frozen_at.is_none())
            {
                a.frozen_at = Some(now);
                // Смерть полоску не убирает - значит рестарт ждёт, пока она
                // пропадёт. Убийство босса её убирает само, там ждать нечего.
                a.restart_needs_gone = by_death;
                self.dirty = true;
            }
        }
    }

    /// Моя смерть в счётчик каждой идущей попытки. Отдельно от заморозки:
    /// в ко-опе бой продолжает союзник, а смерть уже засчитана.
    fn count_death(&mut self) {
        for bar in &self.last_bars {
            if let Some(a) = self
                .attempts
                .iter_mut()
                .find(|a| a.covers(bar.fmg_id, bar.handle.block_id) && a.frozen_at.is_none())
            {
                a.deaths += 1;
                self.dirty = true;
            }
        }
    }

    /// Текущий бой в снимок. При нескольких полосках (Годскин Дуо) берём
    /// первую - ponytail: на HUD всё равно одна строка.
    fn fill_current_fight(&self, s: &mut Snapshot) {
        // Полоски этого кадра, а после убийства - прошлого: `BOSS_LINGER`
        // держит убитого на экране.
        let live = if self.bars.is_empty() { &self.last_bars } else { &self.bars };
        let Some(bar) = live.first() else {
            return;
        };
        // Двойной босс - две полоски и одна попытка на двоих, значит и имя одно
        // на двоих: перечисляем оба (жалоба 2026-08-21 - показывалось только
        // первое). Одинаковые имена у близнецов схлопываем: «Гвардеец ·
        // Гвардеец» читалось бы как ошибка мода.
        let mut names: Vec<String> = Vec::new();
        for name in live.iter().filter_map(|b| b.name.clone()) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        // Через перевод строки, а не разделителем в строку: второе имя должно
        // стоять СТРОЧКОЙ НИЖЕ (прямой запрос 2026-08-21), а оба вывода умеют
        // разбить строку сами - в JSON `\n` экранируется как есть.
        s.boss_name = (!names.is_empty()).then(|| names.join("\n"));
        let Some(a) = self.attempts.iter().find(|a| a.covers(bar.fmg_id, bar.handle.block_id))
        else {
            return;
        };
        // Строка на экране называется «СМЕРТЕЙ» и рисуется как `attempts - 1`
        // (то же и в вебе), поэтому кладём сюда смерти, а не попытки: тогда
        // цифра меняется сразу после смерти, а не по возвращении к боссу.
        s.attempts = a.deaths + 1;
        s.attempt_secs = a
            .frozen_at
            .unwrap_or_else(Instant::now)
            .duration_since(a.started_at)
            .as_secs_f64();
    }

    fn poll_bosses(&mut self) {
        // Таймер стоит перед построением реестра, а не после: `build_registry`
        // зовёт `SoloParamRepository::instance()`, и пока парамы не загружены,
        // каждая попытка снова идёт через поиск синглтона. Раз в полсекунды -
        // достаточно, каждый кадр - дорого.
        if self.last_poll.is_some_and(|t| t.elapsed() < BOSS_POLL) {
            return;
        }
        self.last_poll = Some(Instant::now());

        if self.registry.is_none() {
            self.registry = build_registry();
        }
        if self.regions.is_none() {
            self.regions = build_regions();
        }
        let Some(registry) = self.registry.as_ref() else {
            return;
        };

        let Ok(efm) = (unsafe { CSEventFlagMan::instance() }) else {
            return;
        };
        let mut c = (0u32, 0u32, 0u32, 0u32);
        for e in registry.iter() {
            let killed = efm.virtual_memory_flag.get_flag(e.flag);
            c.3 += 1;
            c.2 += killed as u32;
            if e.named {
                c.1 += 1;
                c.0 += killed as u32;
            }
        }
        self.counts = c;
    }
}

// ---------------------------------------------------------------------------
// Сохранение между запусками
// ---------------------------------------------------------------------------
//
// Попыток на боссах и стрика без смертей в сейве игры нет, поэтому без файла
// они обнуляются при каждом перезаходе - именно это и было главной претензией
// к предыдущему моду. Всё остальное (смерти, уровень, руны, время, NG+, флаги
// убитых боссов) игра хранит сама, и дублировать это здесь нечего.

/// Одна попытка в текстовом виде. Имя идёт последним, поэтому `|` внутри него
/// ничего не ломает.
///
/// `running` (0/1) - шёл ли бой ещё, когда сохраняли (`frozen_at.is_none()`).
/// Без этого поля восстановленная попытка всегда приходила замороженной, и
/// перезапуск игры/мода посреди боя (а не после смерти или убийства) считал
/// следующее появление той же полоски новой попыткой - живьём это выглядело
/// как "одна попытка идёт за две" при каждом перезапуске для теста мода.
fn encode_attempt(a: &Attempt, now: Instant) -> String {
    let secs = a.frozen_at.unwrap_or(now).saturating_duration_since(a.started_at).as_secs_f64();
    let ids: Vec<String> = a.fmg_ids.iter().map(|i| i.to_string()).collect();
    let running = i32::from(a.frozen_at.is_none());
    format!(
        "attempt = {}|{}|{}|{}|{:.1}|{}|{}",
        i32::from(a.block),
        ids.join(","),
        a.count,
        a.deaths_at_start,
        secs,
        running,
        a.name.as_deref().unwrap_or("")
    )
}

/// Разбор строки `attempt = ...`. Битая строка пропускается, а не роняет
/// загрузку: файл лежит рядом с DLL и правится руками.
///
/// Поле `running` добавлено после первого релиза - старые строки (6 полей,
/// без него) разбираются как раньше: всегда замороженными. Это тот же
/// результат, что и раньше, просто по умолчанию, а не полноценный флаг.
fn decode_attempt(value: &str, now: Instant) -> Option<Attempt> {
    let parts: Vec<&str> = value.splitn(7, '|').collect();
    if parts.len() < 5 {
        return None;
    }
    let block = BlockId::from(parts[0].trim().parse::<i32>().ok()?);
    let fmg_ids: Vec<i32> = parts[1].split(',').filter_map(|s| s.trim().parse().ok()).collect();
    if fmg_ids.is_empty() {
        return None;
    }
    let count = parts[2].trim().parse().ok()?;
    let deaths_at_start = parts[3].trim().parse().ok()?;
    let secs: f64 = parts[4].trim().parse().ok()?;
    // Имя (и до него `running`) необязательны - их отсутствие не повод терять
    // счётчик. `parts.len()` отличает три формата: 5 полей (совсем старое,
    // без имени), 6 (старое, с именем, без `running`), 7 (текущее).
    let (running, name) = match parts.len() {
        n if n >= 7 => (parts[5].trim() == "1", parts[6]),
        6 => (false, parts[5]),
        _ => (false, ""),
    };
    let name = name.trim();
    let name = (!name.is_empty()).then(|| name.to_string());

    // Часы показывают ровно сохранённую длительность. Дальше два случая:
    // бой был завершён (смерть/убийство) - восстанавливаем замороженным,
    // следующая полоска этого босса начнёт новую попытку, как раньше; бой
    // ещё шёл (`running`) - НЕ замораживаем, тот же самый бой продолжается
    // бесшовно, и появление той же полоски не бьёт по счётчику лишний раз.
    // ponytail: пока игра была закрыта, часы у продолженного боя всё равно
    // тикают (`started_at` в прошлом) - секунды на экране прыгнут на реальный
    // простой. Мельче, чем удвоенный счётчик попыток, чинить не сейчас.
    let dur = Duration::from_secs_f64(secs.max(0.0));
    Some(Attempt {
        fmg_ids,
        block,
        count,
        started_at: now.checked_sub(dur).unwrap_or(now),
        frozen_at: if running { None } else { Some(now) },
        // Восстановленную попытку ждать не заставляем: полоски на экране нет
        // по определению - игру только что запустили.
        restart_needs_gone: false,
        // В файле смертей нет - берём то, что показывалось и раньше.
        deaths: count.saturating_sub(1),
        deaths_at_start,
        name,
    })
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Collector {
    /// Весь файл целиком: секция на персонажа, чужие секции переносятся как
    /// есть. Иначе игра за второго персонажа стирала бы статистику первого.
    fn save(&mut self) {
        self.dirty = false;
        self.last_save = Instant::now();
        let (Some(path), Some(name)) = (dll_sibling(self.hmodule, STATS_FILE), self.character.clone()) else {
            return;
        };

        let now = Instant::now();
        let mut section = format!("[{name}]
");
        // Стрик хранится как момент смерти в unix-времени: `Instant` между
        // запусками бессмыслен.
        let died_ago = self.last_death_at.elapsed().as_secs();
        section.push_str(&format!("last_death = {}
", unix_now().saturating_sub(died_ago)));
        if self.viewer_deaths > 0 {
            section.push_str(&format!("viewer_deaths = {}
", self.viewer_deaths));
        }
        for a in &self.attempts {
            section.push_str(&encode_attempt(a, now));
            section.push('\n');
        }

        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let text = replace_section(&existing, &name, &section);

        crate::config::write_atomic(&path, &text);
    }

    /// Зритель довёл до смерти. Решает это `StreamHud` - только он знает про
    /// заспавненных врагов и про недавние покупки, - а счёт и файл наши.
    pub fn add_viewer_death(&mut self) {
        self.viewer_deaths += 1;
        self.dirty = true;
    }

    fn load(&mut self) {
        self.attempts.clear();
        self.viewer_deaths = 0;
        let (Some(path), Some(name)) = (dll_sibling(self.hmodule, STATS_FILE), self.character.clone()) else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let now = Instant::now();
        for line in section_lines(&text, &name) {
            let Some((key, value)) = line.split_once('=') else { continue };
            match key.trim() {
                "attempt" => {
                    if let Some(a) = decode_attempt(value, now) {
                        self.attempts.push(a);
                    }
                }
                "viewer_deaths" => self.viewer_deaths = value.trim().parse().unwrap_or(0),
                "last_death" => {
                    if let Ok(stamp) = value.trim().parse::<u64>() {
                        let ago = Duration::from_secs(unix_now().saturating_sub(stamp));
                        self.last_death_at = now.checked_sub(ago).unwrap_or(now);
                    }
                }
                _ => {}
            }
        }
        // Кап применяется и к загруженному: файл могли править руками.
        let excess = self.attempts.len().saturating_sub(MAX_ATTEMPTS);
        if excess > 0 {
            self.attempts.drain(0..excess);
        }
    }
}

/// Непустые строки секции `[name]`, до следующего заголовка.
fn section_lines<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let header = format!("[{name}]");
    let mut inside = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            inside = t == header;
            continue;
        }
        if inside && !t.is_empty() && !t.starts_with(';') {
            out.push(t);
        }
    }
    out
}

/// Заменяет секцию `[name]` на `section`, не трогая остальные - у каждого
/// персонажа своя, и запись за одного не должна стирать другого.
fn replace_section(text: &str, name: &str, section: &str) -> String {
    let header = format!("[{name}]");
    let mut out = String::new();
    let mut skipping = false;
    let mut replaced = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            if t == header {
                skipping = true;
                replaced = true;
                out.push_str(section);
                continue;
            }
            skipping = false;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        if !out.is_empty() && !out.ends_with("

") {
            out.push('\n');
        }
        out.push_str(section);
    }
    out
}

/// Имя персонажа в игре (не имя аккаунта Steam), до первого нуля.
fn read_character_name(data: &PlayerGameData) -> Option<String> {
    let base = (data as *const PlayerGameData as usize + PLAYER_CHARACTER_NAME_OFFSET) as *const u16;
    let units = unsafe { std::slice::from_raw_parts(base, PLAYER_CHARACTER_NAME_LEN) };
    let len = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    (len > 0).then(|| String::from_utf16_lossy(&units[..len]))
}

/// Открыто ли игровое меню. Зеркалим собственный HUD игры: инвентарь, карта,
/// пауза и отдых у благодати выводят её HP/FP/выносливость из `Default` -
/// значит и нашей панели там не место.
///
/// Падает закрыто (не прочитали = считаем, что меню открыто): панель, застрявшая
/// невидимой, - это громко и сразу заметно, а мелькнувшая поверх меню выглядит
/// поломкой. Катсцены `hud_state` в `Default` не выводят, поэтому у них своя
/// проверка (`read_in_cutscene`).
fn read_menu_open() -> bool {
    unsafe { CSFeManImp::instance() }
        .map(|fe| fe.hud_state != CSFeManHudState::Default)
        .unwrap_or(true)
}

fn read_in_cutscene() -> bool {
    unsafe { CSEventFlagMan::instance() }
        .ok()
        .map(|m| {
            let blocks = m.virtual_memory_flag.flag_blocks as *const u8;
            blocks.is_null() || unsafe { *blocks.add(IN_CUTSCENE_BYTE) != 0 }
        })
        .unwrap_or(true)
}

/// Крепость Круглого стола - `m11_10_00_00`.
const HUB_AREA: u8 = 11;
const HUB_BLOCK: u8 = 10;

/// `(мёртв, все игроки мертвы, в Крепости Круглого стола)`.
///
/// Оба ответа берутся за один резолв `WorldChrMan`: он идёт через рефлексию
/// Dantelion2, и звать его дважды за кадр - тот же класс ошибки, что уже ронял
/// игру до 8 FPS.
///
/// Отсутствие игрока (идёт загрузка) не считается ни смертью - иначе каждый
/// переход карты замораживал бы часы попытки, - ни хабом: там покупки и так
/// не исполняются, потому что это не геймплей.
fn read_player() -> (bool, bool, bool) {
    let Some(w) = unsafe { WorldChrMan::instance() }.ok() else {
        return (false, false, false);
    };
    let Some(p) = w.main_player.as_ref() else {
        return (false, false, false);
    };
    // Именно `block_id_origin`, а не `field_ins_handle.block_id`: у главного
    // игрока последний равен -1 (он не принадлежит блоку карты), и хаб не
    // определялся НИКОГДА - покупки в Круглом столе исполнялись как обычно.
    // Найдено 2026-08-27 диагностикой босс-листа, где та же ошибка выглядела
    // как «фильтр Рядом ничего не показывает».
    let block = p.chr_ins.block_id_origin();
    let dead = p.chr_ins.modules.data.hp <= 0;
    // Смерть - это hp <= 0 и в ко-опе тоже: фантом умирает без экрана смерти
    // и без прибавки к счётчику смертей игры.
    //
    // Полный вайп - все, кто есть в наборе игроков. Набор пуст (соло, загрузка)
    // - смотрим на себя.
    let mut seen = false;
    let mut everyone_down = true;
    for pl in w.player_chr_set.characters() {
        seen = true;
        if pl.chr_ins.modules.data.hp > 0 {
            everyone_down = false;
            break;
        }
    }
    let all_dead = if seen { everyone_down } else { dead };
    (dead, all_dead, block.area() == HUB_AREA && block.block() == HUB_BLOCK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eldenring::cs::{FieldInsSelector, FieldInsType};

    fn handle(block: u8, index: u32) -> FieldInsHandle {
        FieldInsHandle {
            selector: FieldInsSelector::from_parts(FieldInsType::Chr, 0, index),
            block_id: BlockId::from_parts(10, block, 0, 0),
        }
    }

    fn bar(block: u8, index: u32, fmg_id: i32) -> BossBar {
        BossBar {
            handle: handle(block, index),
            fmg_id,
            name: Some(format!("boss{fmg_id}")),
            name_fmg: fmg_id,
            name_pending_since: None,
        }
    }

    /// Повторяет ровно ту последовательность, которой `update_bars` двигает
    /// `bars` -> `last_bars`. Держать в согласии с ней.
    fn set_bars(c: &mut Collector, bars: Vec<BossBar>) {
        if !c.bars.is_empty() {
            c.last_bars = c.bars.clone();
        }
        c.bars = bars;
    }

    fn count_of(c: &Collector, b: &BossBar) -> Option<u32> {
        c.attempts
            .iter()
            .find(|a| a.covers(b.fmg_id, b.handle.block_id))
            .map(|a| a.count)
    }

    /// Строка без нуля в конце обязана кончиться сама.
    ///
    /// `MenuString::to_string()` крейта на такой шёл бы, пока не наткнулся на
    /// ноль или на неотображённую страницу, - на игровом потоке это и было
    /// зависание при появлении полоски босса (живьём 2026-08-28, игра 2.7.0.0).
    /// Тест повесил бы прогон целиком, если бы предел пропал.
    #[test]
    fn an_unterminated_name_still_ends() {
        let buf: Vec<u16> = vec![b'A' as u16; msg::MAX_CHARS * 4];
        let got = static_name(buf.as_ptr()).expect("буфер читаемый, имя должно прочитаться");
        assert_eq!(got.chars().count(), msg::MAX_CHARS, "длина обязана упереться в потолок");
    }

    /// Мусорный указатель - это «имени нет», а не падение и не проход по всей
    /// памяти. Ноль и мелкие числа реально приходят из недогруженной игры.
    #[test]
    fn a_bad_pointer_is_just_no_name() {
        assert_eq!(static_name(std::ptr::null()), None);
        assert_eq!(static_name(0x10 as *const u16), None);
        assert_eq!(static_name(0x7FFF_FFFF_0000usize as *const u16), None);
    }

    #[test]
    fn first_bar_starts_attempt_one() {
        let mut c = Collector::new(0);
        let b = bar(1, 0, 100);
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(0, false, false);
        assert_eq!(count_of(&c, &b), Some(1));
    }

    /// Строка «СМЕРТЕЙ» обязана дёрнуться в момент смерти, а не когда
    /// добежишь до босса заново: счётчик попыток растёт на возвращении, и
    /// цифра на экране висела старой всю дорогу обратно (жалоба 2026-08-23).
    #[test]
    fn death_counter_moves_at_the_death_not_at_the_return() {
        let mut c = Collector::new(0);
        let b = bar(1, 0, 100);
        let deaths_of = |c: &Collector| c.attempts.first().map(|a| a.deaths);

        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(0, false, false);
        assert_eq!(deaths_of(&c), Some(0));

        // Умер - полоска в соло не пропадает, но смерть уже засчитана.
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(1, true, true);
        assert_eq!(deaths_of(&c), Some(1), "смерть видно сразу");

        // Экран смерти длинный: по фронту, а не каждый кадр.
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(1, true, true);
        assert_eq!(deaths_of(&c), Some(1), "второй раз за ту же смерть не считаем");

        // Вернулся: попытка новая, а смертей по-прежнему одна.
        set_bars(&mut c, Vec::new());
        c.update_attempts(1, false, false);
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(1, false, false);
        assert_eq!(count_of(&c, &b), Some(2));
        assert_eq!(deaths_of(&c), Some(1), "возвращение - не смерть");
    }

    /// Смерть замораживает часы, возвращение к полоске - новая попытка.
    #[test]
    fn death_then_return_is_a_second_attempt() {
        let mut c = Collector::new(0);
        let b = bar(1, 0, 100);

        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(0, false, false);

        // Умер: полоска в соло НЕ пропадает, она всё ещё в кадре.
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(1, true, true);
        assert_eq!(count_of(&c, &b), Some(1), "смерть сама по себе не новая попытка");

        // Ещё кадр мёртвым - счётчик всё ещё не должен дёрнуться.
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(1, true, true);
        assert_eq!(count_of(&c, &b), Some(1));

        // **Баг 2026-08-21.** Игра вернула игроку HP раньше, чем убрала
        // полоску (это экран загрузки после «Продолжить», а не возвращение к
        // боссу). Пока полоска та же самая и ни разу не пропадала, новой
        // попытки быть не может.
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(1, false, false);
        assert_eq!(count_of(&c, &b), Some(1), "живой HP при висящей полоске - ещё не возвращение");

        // Арена выгрузилась: полоски нет.
        set_bars(&mut c, Vec::new());
        c.update_attempts(1, false, false);

        // Вернулся живым к тому же боссу - вот теперь вторая попытка.
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(1, false, false);
        assert_eq!(count_of(&c, &b), Some(2));
    }

    /// Двойной босс: на панели должны стоять оба имени, а не первое попавшееся.
    #[test]
    fn twin_names_are_both_shown() {
        let mut c = Collector::new(0);
        let (a, b) = (bar(1, 0, 100), bar(1, 1, 101));
        set_bars(&mut c, vec![a, b]);
        c.update_attempts(0, false, false);

        let mut s = Snapshot::default();
        c.fill_current_fight(&mut s);
        assert_eq!(s.boss_name.as_deref(), Some("boss100
boss101"), "второе имя - на своей строке");
        assert_eq!(s.attempts, 1, "имена перечислены, а попытка по-прежнему одна");
    }

    /// Близнецы с одинаковым именем не должны читаться как «Гвардеец Гвардеец».
    #[test]
    fn identical_twin_names_collapse() {
        let mut c = Collector::new(0);
        let mut left = bar(1, 0, 100);
        let mut right = bar(1, 1, 101);
        left.name = Some("Гвардеец".into());
        right.name = Some("Гвардеец".into());
        set_bars(&mut c, vec![left, right]);
        c.update_attempts(0, false, false);

        let mut s = Snapshot::default();
        c.fill_current_fight(&mut s);
        assert_eq!(s.boss_name.as_deref(), Some("Гвардеец"));
    }

    /// Радагон -> Элден Бист: другой FMG id посреди боя в том же блоке.
    /// Это та же попытка, а не новый босс со счётчиком 1.
    #[test]
    fn phase_rename_keeps_the_same_attempt() {
        let mut c = Collector::new(0);
        let phase1 = bar(2, 0, 100);
        let phase2 = bar(2, 0, 200);

        set_bars(&mut c, vec![phase1.clone()]);
        c.update_attempts(0, false, false);
        set_bars(&mut c, vec![phase2.clone()]);
        c.update_attempts(0, false, false);

        assert_eq!(c.attempts.len(), 1, "переименование не должно заводить второй бой");
        assert_eq!(count_of(&c, &phase2), Some(1));
        // И под старым именем этот же бой тоже находится.
        assert_eq!(count_of(&c, &phase1), Some(1));
    }

    /// Одинаковые боссы, растыканные по миру (Древо-аватары), делят имя и FMG
    /// id. Счётчик у каждого свой - иначе подход к свежему показывает чужой.
    #[test]
    fn same_boss_in_another_block_counts_separately() {
        let mut c = Collector::new(0);
        let first = bar(1, 0, 100);
        let second = bar(2, 1, 100);

        set_bars(&mut c, vec![first.clone()]);
        c.update_attempts(0, false, false);
        set_bars(&mut c, vec![]);
        c.update_attempts(0, false, false);
        set_bars(&mut c, vec![second.clone()]);
        c.update_attempts(0, false, false);

        assert_eq!(c.attempts.len(), 2);
        assert_eq!(count_of(&c, &second), Some(1), "у свежего аватара свой счёт");
    }

    /// Босс умер - полоска пропала - подошли снова: это вторая попытка,
    /// а не продолжение первой.
    #[test]
    fn bar_disappearing_then_returning_is_a_new_attempt() {
        let mut c = Collector::new(0);
        let b = bar(3, 0, 300);

        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(0, false, false);
        set_bars(&mut c, vec![]);
        c.update_attempts(0, false, false);
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(0, false, false);

        assert_eq!(count_of(&c, &b), Some(2));
    }

    /// Первый заход - ноль смертей; каждый следующий - плюс одна, и суммируется
    /// это по всем боссам, а не только по текущему.
    #[test]
    fn deaths_on_bosses_sum_every_attempt_past_the_first() {
        let mut c = Collector::new(0);
        let a = bar(1, 0, 100);
        let b = bar(2, 0, 200);

        set_bars(&mut c, vec![a.clone()]);
        c.update_attempts(0, false, false); // заход 1 на a - смертей 0
        set_bars(&mut c, vec![]);
        c.update_attempts(0, false, false);
        set_bars(&mut c, vec![a.clone()]);
        c.update_attempts(0, false, false); // заход 2 на a - смерть
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(0, false, false); // заход 1 на b - смертей 0

        let deaths: u32 = c.attempts.iter().map(|a| a.count.saturating_sub(1)).sum();
        assert_eq!(deaths, 1);
    }

    /// `total_attempts` в снимке - это сумма счётчиков по всем боссам, а не
    /// счётчик только текущего боя: комбинируется с числом убитых боссов в
    /// "среднее попыток на босса".
    #[test]
    fn total_attempts_sums_every_boss() {
        let mut c = Collector::new(0);
        let a = bar(1, 0, 100);
        let b = bar(2, 0, 200);

        set_bars(&mut c, vec![a.clone()]);
        c.update_attempts(0, false, false); // попытка 1 на a
        set_bars(&mut c, vec![]);
        c.update_attempts(0, false, false);
        set_bars(&mut c, vec![a.clone()]);
        c.update_attempts(0, false, false); // попытка 2 на a
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(0, false, false); // попытка 1 на b

        let total: u32 = c.attempts.iter().map(|a| a.count).sum();
        assert_eq!(total, 3);
    }

    /// Разные боссы в разных блоках - разные попытки, но список не растёт
    /// бесконечно: он переживает всё и ничем другим не ограничен.
    #[test]
    fn attempts_are_capped() {
        let mut c = Collector::new(0);
        let n = MAX_ATTEMPTS as u32 + 10;
        for i in 0..n {
            let b = bar(i as u8, i, 1000 + i as i32);
            set_bars(&mut c, vec![b]);
            c.update_attempts(0, false, false);
        }
        assert_eq!(c.attempts.len(), MAX_ATTEMPTS);
        // Обрезается начало, значит самый свежий бой на месте.
        let newest = bar((n - 1) as u8, n - 1, 1000 + (n - 1) as i32);
        assert_eq!(count_of(&c, &newest), Some(1));
    }

    /// Двойной босс (Годскин Дуо, Кристалиане): две полоски одновременно -
    /// это ОДИН бой с одним счётчиком, а не две попытки. Проверяем весь цикл:
    /// появление, смерть одного, смерть игрока, возврат.
    #[test]
    fn twin_bosses_share_one_attempt() {
        let mut c = Collector::new(0);
        let (a, b) = (bar(1, 0, 100), bar(1, 1, 101));

        // Обе полоски в одном кадре.
        set_bars(&mut c, vec![a.clone(), b.clone()]);
        c.update_attempts(0, false, false);
        assert_eq!(c.attempts.len(), 1, "две полоски - один бой");
        assert_eq!(count_of(&c, &a), Some(1));
        assert_eq!(count_of(&c, &b), Some(1), "второй босс попадает в ту же попытку");

        // Одного убили - второй ещё дерётся. Бой не закончен, замораживать
        // часы нельзя.
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(0, false, false);
        assert_eq!(count_of(&c, &b), Some(1));
        assert!(c.attempts[0].frozen_at.is_none(), "бой продолжается, пока жив второй");

        // Игрок умер: часы встают, счётчик пока прежний.
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(1, true, true);
        assert_eq!(count_of(&c, &b), Some(1));
        assert!(c.attempts[0].frozen_at.is_some(), "смерть замораживает часы");

        // Арена выгрузилась после смерти - полосок нет.
        set_bars(&mut c, Vec::new());
        c.update_attempts(1, false, false);

        // Подошли снова, обе полоски вернулись - это вторая попытка, ОДНА.
        set_bars(&mut c, vec![a.clone(), b.clone()]);
        c.update_attempts(1, false, false);
        assert_eq!(c.attempts.len(), 1, "счётчик по-прежнему один на двоих");
        assert_eq!(count_of(&c, &a), Some(2));
        assert_eq!(count_of(&c, &b), Some(2), "оба показывают одну и ту же попытку");
    }

    /// Ко-оп: моя смерть считается в счётчик, но часы держит союзник -
    /// останавливает их только полный вайп.
    #[test]
    fn coop_clock_runs_until_everyone_is_down() {
        let mut c = Collector::new(0);
        let b = bar(1, 0, 100);
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(0, false, false);

        // Я лёг, союзник дерётся дальше.
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(0, true, false);
        assert!(c.attempts[0].frozen_at.is_none(), "пока жив союзник, часы идут");
        assert_eq!(c.attempts[0].deaths, 1, "моя смерть засчитана сразу");

        // Лёг и он.
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(0, true, true);
        assert!(c.attempts[0].frozen_at.is_some(), "вайп останавливает часы");
        assert_eq!(c.attempts[0].deaths, 1, "чужая смерть в мой счётчик не идёт");
    }

    /// Убийство двойного босса замораживает попытку один раз, а не по разу на
    /// каждую полоску: иначе показанное время боя считалось бы от чужого
    /// момента.
    #[test]
    fn killing_twins_freezes_once() {
        let mut c = Collector::new(0);
        let (a, b) = (bar(1, 0, 100), bar(1, 1, 101));

        set_bars(&mut c, vec![a.clone(), b.clone()]);
        c.update_attempts(0, false, false);

        // Обе полоски исчезли в одном кадре - босс убит.
        set_bars(&mut c, vec![]);
        c.update_attempts(0, false, false);
        assert_eq!(c.attempts.len(), 1);
        let frozen = c.attempts[0].frozen_at.expect("убийство замораживает часы");

        // Следующий кадр ничего не меняет: замораживать уже нечего.
        c.update_attempts(0, false, false);
        assert_eq!(c.attempts[0].frozen_at, Some(frozen), "второй заморозки нет");
    }

    /// Обе полоски двойного босса обязаны пережить перезапуск одной записью:
    /// иначе после перезахода каждый из них завёл бы свой счётчик.
    #[test]
    fn twin_bosses_survive_a_round_trip() {
        let mut c = Collector::new(0);
        let (a, b) = (bar(1, 0, 100), bar(1, 1, 101));
        set_bars(&mut c, vec![a.clone(), b.clone()]);
        c.update_attempts(0, false, false);

        let now = Instant::now();
        let line = encode_attempt(&c.attempts[0], now);
        let back = decode_attempt(line.split_once('=').unwrap().1, now).expect("строка читается");
        assert!(back.covers(100, a.handle.block_id), "первый босс на месте");
        assert!(back.covers(101, b.handle.block_id), "второй тоже");
    }

    /// Известный потолок логики, зафиксированный намеренно: два разных босса
    /// подряд в одном блоке внутри `PHASE_WINDOW` сливаются в один счётчик -
    /// отличить их от смены фазы (Радагон -> Элден Бист) нечем. Тест стоит
    /// тут, чтобы это заметили, если поведение когда-нибудь изменится.
    #[test]
    fn two_bosses_in_one_block_merge_known_ceiling() {
        let mut c = Collector::new(0);
        set_bars(&mut c, vec![bar(1, 0, 100)]);
        c.update_attempts(0, false, false);
        set_bars(&mut c, vec![bar(1, 1, 200)]);
        c.update_attempts(0, false, false);
        assert_eq!(c.attempts.len(), 1);
    }

    /// Катсцена не должна засчитываться во время попытки.
    #[test]
    fn cutscene_time_is_not_fight_time() {
        let mut c = Collector::new(0);
        let b = bar(5, 0, 500);
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(0, false, false);

        let before = c.attempts[0].started_at;
        c.note_cutscene(true);
        std::thread::sleep(Duration::from_millis(30));
        c.note_cutscene(false);

        let paused = c.attempts[0].started_at.duration_since(before);
        assert!(paused >= Duration::from_millis(25), "пауза не вычтена: {paused:?}");
    }

    /// Попытка должна пережить перезапуск: счётчик, смерти и время боя
    /// возвращаются такими же. Ради этого всё сохранение и делалось.
    #[test]
    fn attempt_survives_a_round_trip() {
        let now = Instant::now();
        let a = Attempt {
            fmg_ids: vec![100, 200],
            block: BlockId::from_parts(10, 3, 0, 0),
            count: 17,
            started_at: now - Duration::from_secs(42),
            frozen_at: Some(now),
            restart_needs_gone: false,
            deaths: 0,
            deaths_at_start: 9,
            name: Some(String::from("Margit | the Fell Omen")),
        };

        let line = encode_attempt(&a, now);
        let value = line.split_once('=').unwrap().1;
        let back = decode_attempt(value, now).expect(&line);

        assert_eq!(back.count, 17);
        assert_eq!(back.deaths_at_start, 9);
        assert_eq!(back.fmg_ids, vec![100, 200]);
        assert_eq!(back.block, a.block);
        // Труба внутри имени не ломает разбор - имя идёт последним полем.
        assert_eq!(back.name.as_deref(), Some("Margit | the Fell Omen"));

        let secs = back.frozen_at.unwrap().duration_since(back.started_at).as_secs_f64();
        assert!((secs - 42.0).abs() < 0.2, "время боя не восстановилось: {secs}");
    }

    /// Восстановленная попытка приходит замороженной, поэтому подход к тому же
    /// боссу после перезахода - это следующая попытка, а не та же самая.
    #[test]
    fn restored_attempt_continues_counting() {
        let now = Instant::now();
        let mut c = Collector::new(0);
        c.attempts.push(decode_attempt("167968768|100|17|9|42.0|Margit", now).unwrap());

        let b = bar(3, 0, 100);
        assert_eq!(b.handle.block_id, BlockId::from(167968768));
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(9, false, false);

        assert_eq!(count_of(&c, &b), Some(18), "счётчик должен продолжиться, а не начаться заново");
    }

    /// Баг, найденный живьём при частых перезапусках во время разработки:
    /// сохранение посреди активного боя (не смерть, не убийство - просто
    /// перезапуск игры/мода) не должно считаться концом попытки. Без поля
    /// `running` восстановленная попытка ВСЕГДА приходила замороженной, и
    /// первое же появление той же полоски после перезапуска засчитывало
    /// лишнюю попытку - "одна попытка идёт за две".
    #[test]
    fn restored_running_attempt_does_not_double_count() {
        let now = Instant::now();
        let mut c = Collector::new(0);
        // `running = 1`: бой ещё шёл, когда сохраняли.
        c.attempts.push(decode_attempt("167968768|100|17|9|42.0|1|Margit", now).unwrap());

        let b = bar(3, 0, 100);
        set_bars(&mut c, vec![b.clone()]);
        c.update_attempts(9, false, false);

        assert_eq!(count_of(&c, &b), Some(17), "тот же бой продолжается - счётчик не должен прыгнуть");
    }

    /// Круговой обход тоже обязан сохранять и восстанавливать `running`, не
    /// только для уже замороженных попыток.
    #[test]
    fn running_attempt_round_trips_as_running() {
        let now = Instant::now();
        let a = Attempt {
            fmg_ids: vec![100],
            block: BlockId::from_parts(10, 3, 0, 0),
            count: 4,
            started_at: now - Duration::from_secs(10),
            frozen_at: None,
            restart_needs_gone: false,
            deaths: 0,
            deaths_at_start: 1,
            name: None,
        };
        let line = encode_attempt(&a, now);
        let value = line.split_once('=').unwrap().1;
        let back = decode_attempt(value, now).expect(&line);
        assert!(back.frozen_at.is_none(), "{line}");
    }

    #[test]
    fn broken_lines_are_skipped_not_fatal() {
        let now = Instant::now();
        assert!(decode_attempt("", now).is_none());
        assert!(decode_attempt("нечисло|100|1|0|0|x", now).is_none());
        assert!(decode_attempt("1||1|0|0|x", now).is_none(), "нет ни одного fmg id");
        assert!(decode_attempt("1|100|1|0", now).is_none(), "полей меньше, чем нужно");
        // Имени может не быть - это не повод терять счётчик.
        assert!(decode_attempt("1|100|4|0|3.5", now).is_some());
    }

    /// Второй персонаж не должен затирать статистику первого.
    #[test]
    fn other_characters_sections_survive_a_write() {
        let file = "[Ivan]
last_death = 100
attempt = 1|100|3|0|5.0|A
[Anna]
attempt = 2|200|7|1|9.0|B
";
        let out = replace_section(file, "Ivan", "[Ivan]
attempt = 1|100|4|0|6.0|A
");

        assert!(out.contains("attempt = 1|100|4|0|6.0|A"), "{out}");
        assert!(!out.contains("attempt = 1|100|3"), "старая строка Ивана осталась: {out}");
        assert!(out.contains("[Anna]"), "секция Анны потерялась: {out}");
        assert!(out.contains("attempt = 2|200|7|1|9.0|B"), "статистика Анны потерялась: {out}");
    }

    /// Новый персонаж дописывается, а не заменяет файл.
    #[test]
    fn new_character_is_appended() {
        let out = replace_section("[Anna]
attempt = 2|200|7|1|9.0|B
", "Ivan", "[Ivan]
last_death = 5
");
        assert!(out.contains("[Anna]") && out.contains("[Ivan]"), "{out}");
    }

    #[test]
    fn section_lines_reads_only_its_own_section() {
        let file = "; комментарий
[Ivan]
last_death = 100
[Anna]
last_death = 200
";
        assert_eq!(section_lines(file, "Ivan"), vec!["last_death = 100"]);
        assert_eq!(section_lines(file, "Anna"), vec!["last_death = 200"]);
        assert!(section_lines(file, "Нет такого").is_empty());
    }

}

