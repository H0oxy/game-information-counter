//! Боссы: кто ещё жив, как его зовут и где он стоит.
//!
//! Всё из парамов, ни одного сырого оффсета и ни одного списка в репозитории:
//! мод с новыми боссами добавляет строки туда же, и список растёт сам.
//!
//! Связка найдена живьём 2026-08-27, после двух провальных заходов:
//!
//! ```text
//! GameAreaParam.defeat_boss_flag_id      кто вообще босс + координаты + карта
//!   -> WorldMapPointParam.text_disable_flag_id1   маркер этого босса на карте
//!        -> text_id1 -> FMG слот 19 (PlaceName)   его имя
//! ```
//!
//! `cleared_event_flag_id`, который выглядит как путь к маркеру, не
//! совпадает НИ РАЗУ (0 против 183). А `found_boss_text_id` из самого
//! `GameAreaParam` именем не является вовсе - он либо не резолвится нигде,
//! либо попадает в чужой FMG по совпадению номера.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use eldenring::cs::{
    BonfireWarpParam, BonfireWarpSubCategoryParam, BonfireWarpTabParam, CSEventFlagMan,
    GameAreaParam, NpcParam, SoloParamRepository, WorldChrMan, WorldMapPointParam,
};
use eldenring::cs::ChrInsExt;
use fromsoftware_shared::FromStatic;

/// Иконка маркера-награды («Великая руна Годрика»). У великих рун маркер
/// висит на том же флаге победы, что и сам босс, и его имя - название руны, а
/// не имя босса. Берём такой текст, только если другого нет.
const REWARD_ICON: u16 = 373;

/// Как часто пересчитывать. Список нужен только с открытым окном, а флаги
/// меняются раз в несколько минут.
const POLL: Duration = Duration::from_secs(1);

/// Босс: то, что не меняется за сессию.
struct Boss {
    flag: u32,
    /// `None` - имени не нашлось ни в одном источнике. Такой босс всё равно
    /// живёт в реестре: без него `learn` не смог бы его найти, а в списке он
    /// показывается заглушкой.
    name: Option<String>,
    /// `m{area}_{block}_{map}` - карта, на которой он стоит.
    map: (u8, u8, u8),
    pos: (f32, f32, f32),
    /// Локация, в которой он стоит, если игра её знает.
    place: Option<String>,
    /// За победу дают воспоминание - обменную награду великих врагов.
    remembrance: bool,
}

/// Строка списка на экране.
#[derive(Clone)]
pub struct Row {
    pub name: String,
    pub place: String,
    /// Метры до босса. Только когда игрок на той же карте: координаты
    /// локальны для неё, и между картами это число ничего не значит.
    pub dist: Option<f32>,
    /// Насколько босс выше (плюс) или ниже (минус) игрока, в метрах.
    pub dy: Option<f32>,
    /// В том же регионе, что и игрок. Именно в регионе, а не на той же карте:
    /// по карте фильтр отсекал вообще всё, стоило зайти в любое подземелье.
    pub here: bool,
    pub killed: bool,
    /// За него дают воспоминание.
    pub remembrance: bool,
    /// Сколько таких же в этой локации. Кристалийцев в одном месте бывает
    /// трое, и три одинаковые строки подряд читаются как ошибка мода.
    pub count: usize,
    /// Карта и координаты самого босса - чтобы пересчитать расстояние, не
    /// пересобирая строку. См. `refresh_distances`.
    map: Cell,
    pos: (f32, f32, f32),
}

impl Row {
    /// Насколько до него добираться: горизонталь и высота вместе.
    fn reach(&self) -> Option<f32> {
        Some(reach(self.dist?, self.dy.unwrap_or(0.0)))
    }
}

/// Расстояние с учётом высоты - им и решается, кто ближе.
///
/// Показываем метры и высоту раздельно (босс под ногами и босс в километре по
/// прямой - разные вещи), а вот ВЫБИРАТЬ по одной горизонтали нельзя: живьём
/// босс в километре под землёй обходил соседа на своём уровне (2026-09-07).
fn reach(dist: f32, dy: f32) -> f32 {
    dist.hypot(dy)
}

/// Всё, что строится из парамов один раз: они в рантайме не меняются.
#[derive(Default)]
struct Registry {
    bosses: Vec<Boss>,
    graces: Vec<Grace>,
}

static ALL: Mutex<Option<Registry>> = Mutex::new(None);
/// Последний посчитанный список и когда его считали.
static LIVE: Mutex<(Vec<Row>, Option<Instant>, f32)> = Mutex::new((Vec::new(), None, 0.0));

/// Имя каждого боссового флага: маркер карты, у которого этот флаг скрывает
/// подпись.
/// То же, плюс клетка маркера на большой карте.
///
/// Подземелью благодать приписана к клетке ОВЕРВОРЛДА, где стоит вход, а не к
/// его собственной карте, - поэтому у части катакомб и пещер своей благодати
/// «не находилось» вовсе (живьём 2026-08-27: 13 боссов под m-кодом). Маркер
/// входа эту клетку и знает.
fn names_and_pins(repo: &SoloParamRepository) -> (HashMap<u32, String>, HashMap<u32, Cell>) {
    /// Насколько источнику имени можно верить. Меньше - лучше.
    type Rank = u8;
    const MARKER: Rank = 0;
    const GRACE: Rank = 1;
    const REWARD: Rank = 2;
    const SIDE_FIELD: Rank = 3;

    let mut out: HashMap<u32, (Rank, String)> = HashMap::new();
    let mut pins: HashMap<u32, Cell> = HashMap::new();
    let mut put = |flag: u32, rank: Rank, name: Option<String>| {
        let (Some(name), true) = (name, flag != 0) else {
            return;
        };
        match out.get(&flag) {
            Some((have, _)) if *have <= rank => {}
            _ => {
                out.insert(flag, (rank, name));
            }
        }
    };

    for (_, r) in repo.rows::<WorldMapPointParam>() {
        let name = crate::msg::text_layered(crate::msg::PLACE_NAME, r.text_id1());
        // Наградный маркер («Великая руна Годрика») висит на том же флаге, что
        // и сам босс, и назвал бы его руной. Уступает любому другому.
        let rank = if r.icon_id() == REWARD_ICON { REWARD } else { MARKER };
        put(r.text_disable_flag_id1(), rank, name.clone());
        if r.text_disable_flag_id1() != 0 {
            pins.entry(r.text_disable_flag_id1())
                .or_insert((r.area_no(), r.grid_x_no(), r.grid_z_no()));
        }

        // Остальные семь полей - запасной путь: у части маркеров флаг победы
        // лежит не в первом. Верим им меньше всего, потому что совпадение
        // может быть и случайным.
        for flag in [
            r.text_disable_flag_id2(),
            r.text_disable_flag_id3(),
            r.text_disable_flag_id4(),
            r.cleared_event_flag_id(),
        ] {
            put(flag, SIDE_FIELD, name.clone());
        }
    }

    // Благодать рядом с ареной обычно носит имя самого босса («Годрик
    // Сторукий», «Маргит, Ужасное Знамение»), и её флаг - это флаг победы.
    // Источник независимый от карты, поэтому им закрываются боссы, которым
    // маркер не завели вовсе.
    for (_, r) in repo.rows::<BonfireWarpParam>() {
        let name = crate::msg::text_layered(crate::msg::PLACE_NAME, r.text_id1());
        put(r.text_disable_flag_id1(), GRACE, name.clone());
        put(r.cleared_event_flag_id(), GRACE, name);
    }

    (out.into_iter().map(|(f, (_, n))| (f, n)).collect(), pins)
}

/// Клетка карты: `m{area}_{block}_{map}` он же `{area}/{gridX}/{gridZ}`.
type Cell = (u8, u8, u8);

/// Карта, на которой стоит существо с таким entity id.
///
/// Entity id в Elden Ring начинается с номера карты: `10001950` - это
/// `m10_00_00`. Клетка у благодати подземелья указывает на ВХОД в открытом
/// мире, а не на саму карту, поэтому у катакомб своей благодати не находилось
/// вовсе - живьём 2026-08-27 тринадцать боссов остались под m-кодом.
fn cell_of_entity(id: u32, expect_area: u8) -> Option<Cell> {
    // У открытого мира entity id длиннее восьми цифр, и разбирать его этой
    // формулой нельзя - номер зоны получился бы из середины числа.
    if id >= 100_000_000 {
        return None;
    }
    let (area, block) = ((id / 1_000_000) as u8, ((id / 10_000) % 100) as u8);
    // Отдельные карты игры лежат в этом диапазоне зон. Всё, что вне его, -
    // мусор: служебная строка с семизначным `4001950` иначе читается как зона
    // 4, и благодать уезжает на несуществующую карту (живьём 2026-08-27 это
    // подняло число боссов без места с 13 до 80).
    if !(10..=45).contains(&area) {
        return None;
    }
    // Зона должна совпасть с той, что игра назвала сама, - ЛИБО игра назвала
    // клетку открытого мира. Второе и есть случай подземелья: благодать
    // катакомб приписана к клетке ВХОДА, а к какой карте она ведёт, знает
    // только entity id.
    (area == expect_area || tiled(expect_area)).then_some((area, block, 0))
}

/// Где стоит благодать и к какой локации игра её отнесла.
struct Grace {
    cell: Cell,
    pos: (f32, f32, f32),
    place: String,
}

/// Локации так, как их делит сама игра в меню телепорта.
///
/// Это и есть ответ на «где босс»: подкатегория благодати - «Замок Штормвейл»,
/// «Святое древо Микеллы». Вкладка (`tab`) крупнее - «Лимгрейв», - и берётся,
/// только если у подкатегории нет своего текста.
///
/// `WorldMapPlaceNameParam`, который стоял тут раньше, для этого не годится:
/// подписей в нём десяток на всю игру, среди них нет ни Лимгрейва, ни Кэлида,
/// и ближайшая отвечала наугад - живьём 2026-08-27 Годрик из `m60_39_50`
/// оказался на «Плато Альтус», а Маления в «Вершинах великанов».
fn graces(repo: &SoloParamRepository) -> Vec<Grace> {
    // Слот у этих текстов свой и заранее неизвестен - ищем по всем. Строк
    // здесь несколько десятков, и реестр строится один раз.
    let tabs: HashMap<u16, String> = repo
        .rows::<BonfireWarpTabParam>()
        .filter_map(|(id, r)| Some((id as u16, crate::msg::text_anywhere(r.text_id())?)))
        .collect();
    let subs: HashMap<i32, String> = repo
        .rows::<BonfireWarpSubCategoryParam>()
        .filter_map(|(id, r)| {
            let name = crate::msg::text_anywhere(r.text_id())
                .or_else(|| tabs.get(&r.tab_id()).cloned())?;
            Some((id as i32, name))
        })
        .collect();

    repo.rows::<BonfireWarpParam>()
        .filter_map(|(_, r)| {
            let cell = (r.area_no(), r.grid_x_no(), r.grid_z_no());
            Some(Grace {
                // Для подземелья карта берётся из entity id: клетка там - это
                // вход на большой карте, а не сама карта.
                cell: cell_of_entity(r.bonfire_entity_id(), cell.0).unwrap_or(cell),
                pos: (r.pos_x(), r.pos_y(), r.pos_z()),
                place: subs.get(&r.bonfire_sub_category_id())?.clone(),
            })
        })
        .collect()
}

/// Сторона тайла открытого мира в метрах. Координаты внутри `m60_XX_YY`
/// локальны для тайла, и без этого шага соседние тайлы неразличимы.
const TILE: f32 = 256.0;

/// Зоны, нарезанные на тайлы: там клетка - это координата, а не отдельная
/// карта. 60 - Междуземье, 61 - Земли Теней.
fn tiled(area: u8) -> bool {
    area >= 60
}

/// Координата в пределах зоны. У тайлов - сквозная, у отдельных карт -
/// локальная, и сравнивать их можно только внутри одной карты.
fn flat(cell: Cell, pos: (f32, f32, f32)) -> (f32, f32) {
    if tiled(cell.0) {
        (cell.1 as f32 * TILE + pos.0, cell.2 as f32 * TILE + pos.2)
    } else {
        (pos.0, pos.2)
    }
}

/// Метры между двумя точками, если их вообще можно сравнивать.
///
/// В открытом мире координаты сквозные (см. `flat`), поэтому расстояние
/// считается через любые тайлы одной зоны. У отдельных карт координаты
/// локальные: сравнивать их можно только внутри своей карты, у соседней те же
/// числа означают другое место.
fn distance(
    from: Cell,
    from_pos: (f32, f32, f32),
    to: Cell,
    to_pos: (f32, f32, f32),
) -> Option<(f32, f32)> {
    let comparable = if tiled(from.0) { tiled(to.0) && from.0 == to.0 } else { from == to };
    if !comparable {
        return None;
    }
    let (a, b) = (flat(from, from_pos), flat(to, to_pos));
    // Расстояние ПО ГОРИЗОНТАЛИ, высота отдельным числом: босс в подземелье
    // прямо под ногами и босс в километре по прямой - разные вещи, а одна
    // цифра их смешивала (прямой запрос 2026-08-27).
    //
    // Высота сравнима везде, где сравнимо расстояние: сетка тайлов двумерная
    // (block = gridX, region = gridZ), вертикального номера в ней нет, значит
    // и своего начала отсчёта по Y у тайла быть не может. Прежний запрет
    // («вечное +89») родился из havok-координат игрока - см. `player_at`.
    Some((
        ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt(),
        to_pos.1 - from_pos.1,
    ))
}

/// Локация босса: ближайшая к нему благодать.
///
/// В открытом мире «ближайшая» считается по расстоянию, а не по номеру клетки:
/// у тайла может не быть своей благодати, и сравнение номеров уводило босса
/// через полкарты - живьём 2026-08-27 Годрик из `m60_39_50` оказался на
/// «Тракте Беллума».
///
/// В отдельных картах (замки, подземелья, катакомбы) сравнивать можно только
/// внутри своей: координаты там локальные, и у соседней карты они означают
/// совсем другое место.
fn place_of(map: Cell, pos: (f32, f32, f32), graces: &[Grace]) -> Option<String> {
    let me = flat(map, pos);
    let fits = |g: &&Grace| {
        if tiled(map.0) { tiled(g.cell.0) && g.cell.0 == map.0 } else { g.cell == map }
    };
    let nearest = graces
        .iter()
        .filter(fits)
        .min_by(|a, b| {
            let d = |g: &Grace| {
                let p = flat(g.cell, g.pos);
                (p.0 - me.0).powi(2) + (p.1 - me.1).powi(2)
            };
            d(a).partial_cmp(&d(b)).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|g| g.place.clone());
    if nearest.is_some() {
        return nearest;
    }

    // Благодати на карте может не быть вовсе - Церковь Упования, две карты
    // Нокрона. Связки «карта -> её название» в парамах при этом НЕТ: игра
    // ставит надпись из событий карты, а парамы знают место только через
    // объекты на нём.
    //
    // Зато соседняя карта той же зоны - это, как правило, оно и есть: зона 12
    // целиком Нокрон, зона 10 целиком Штормвейл. Берём ближайшую по номеру
    // блока. Открытого мира это не касается: там пустых клеток нет, а зона
    // одна на пол-игры.
    if tiled(map.0) {
        return None;
    }
    graces
        .iter()
        .filter(|g| g.cell.0 == map.0)
        .min_by_key(|g| g.cell.1.abs_diff(map.1) as u32)
        .map(|g| g.place.clone())
}

/// Полный реестр: строка `GameAreaParam` плюс имя из маркера карты.
fn build(repo: &SoloParamRepository) -> Registry {
    let (names, pins) = names_and_pins(repo);
    // Снимок, а не удержание: замок отпускается этой же строкой. `build()`
    // зовут из `registry()`, а вызывающий может держать `LEARNED` сам - и
    // тогда удержание здесь было бы дедлоком (см. `learn`).
    let learned = LEARNED.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
    let graces = graces(repo);
    let rewards = remembrance_flags();
    let bosses = repo
        .rows::<GameAreaParam>()
        .filter(|(_, r)| r.defeat_boss_flag_id() != 0)
        .map(|(_, r)| {
            let flag = r.defeat_boss_flag_id();
            let map = (r.boss_map_area_no(), r.boss_map_block_no(), r.boss_map_map_no());
            let pos = (r.boss_pos_x(), r.boss_pos_y(), r.boss_pos_z());
            Boss {
                flag,
                // Выученное в бою важнее всего - см. `learn`. Дальше таблица,
                // и только потом маркеры карты: подпись маркера - это НАЗВАНИЕ
                // МЕСТА, и боссу она достаётся лишь по совпадению флага. В
                // ванили таких совпадений два на 472 маркера, а мод, который
                // флаги маркерам вернул, иначе переименовывал бы боссов в
                // «Форт порицания» (живьём 2026-08-28, The Convergence).
                name: learned
                    .get(&flag)
                    .cloned()
                    .or_else(|| table_name(repo, flag))
                    .or_else(|| names.get(&flag).cloned()),
                map,
                pos,
                // Своей благодати у карты может не быть - тогда спрашиваем по
                // клетке входа на большой карте.
                place: place_of(map, pos, &graces).or_else(|| {
                    let pin = pins.get(&flag)?;
                    place_of(*pin, (0.0, 0.0, 0.0), &graces)
                }),
                remembrance: rewards.contains(&flag),
            }
        })
        .fold(Vec::<Boss>::new(), one_per_flag);
    Registry { bosses, graces }
}

/// Один флаг победы - один босс.
///
/// Вторая строка на тот же флаг обычно описывает вход на большой карте (у
/// Годрика это тайл Лимгрейва рядом с замком), и в списке он появлялся дважды.
/// Побеждает отдельная карта: там босс стоит физически, а не значком на карте.
fn one_per_flag(mut acc: Vec<Boss>, b: Boss) -> Vec<Boss> {
    match acc.iter_mut().find(|o| o.flag == b.flag) {
        Some(old) if tiled(old.map.0) && !tiled(b.map.0) => *old = b,
        Some(_) => {}
        None => acc.push(b),
    }
    acc
}

/// Где стоит игрок: карта, относительно которой заданы его координаты, и они
/// сами.
///
/// **`physics.position` здесь НЕ годится** - это havok-координаты, начало у
/// них своё и плавает вместе с загруженной областью, а у боссов и благодатей в
/// парамах координаты блочные. Сравнение двух разных систем и давало «116 м до
/// босса, рядом с которым стоишь» и постоянный сдвиг по высоте (живьём
/// 2026-08-29). `PlayerIns::block_position` - та же система, что и в парамах.
///
/// **`field_ins_handle.block_id` для главного игрока НЕ годится** - он равен
/// -1 (живьём 2026-08-27: диагностика показала `m255_255_255`), потому что сам
/// игрок не принадлежит ни одному блоку карты. Нужен `block_id_origin` - тот
/// блок, ОТ КОТОРОГО отсчитаны координаты, то есть тайл открытого мира или
/// карта подземелья.
fn player_at() -> Option<(Cell, (f32, f32, f32))> {
    let w = unsafe { WorldChrMan::instance() }.ok()?;
    let p = w.main_player.as_ref()?;
    let b = match i32::from(p.current_block_id) {
        -1 => p.chr_ins.block_id_origin(),
        _ => p.current_block_id,
    };
    let pos = p.block_position;
    Some(((b.area(), b.block(), b.region()), (pos.x, pos.y, pos.z)))
}

/// Реестр из кэша, при первом обращении - из парамов.
///
/// Недостроенный НЕ запоминаем - иначе список остался бы таким до конца
/// сессии. «Недостроенный» - это не только пустой: строки `GameAreaParam`
/// игра поднимает РАНЬШЕ, чем тексты, и реестр, собранный в это окно, выходит
/// полным боссами и пустым именами. Живьём 2026-08-27 (игра 2.7.0.0) он таким
/// и кэшировался: панель считала 212 боссов, а список показывал ноль.
///
/// ponytail: пока текстов нет вовсе, это пересборка раз в `POLL` - дорого, но
/// только с открытым окном списка, и само окно без имён всё равно бесполезно.
fn registry(slot: &mut Option<Registry>) -> Option<&Registry> {
    if slot.as_ref().is_none_or(|r| r.bosses.is_empty()) {
        let repo = unsafe { SoloParamRepository::instance() }.ok()?;
        let built = build(repo);
        if built.bosses.is_empty() {
            return None;
        }
        *slot = Some(built);
    }
    slot.as_ref()
}

/// Имена боссов: `флаг победы -> id имени в текстах игры` либо `-> "Имя"`.
///
/// **Нужна с версии игры 2.7.0.0.** До неё имя бралось из маркера карты по
/// флагу победы; замер 2026-08-27 показал, что этой связки в парамах больше
/// нет ВООБЩЕ - ни в маркерах (два ненулевых флага на 472 строки), ни в
/// благодатях, ни в одном из 239 парамов игры. Сопоставление по рунам
/// (`bonus_soul_single` против `NpcParam.get_soul`) тоже не спасает: точных
/// совпадений 4 из 207.
///
/// Ключ - id строки `GameAreaParam`, и он же флаг победы (замерено там же),
/// поэтому связка «флаг -> босс» берётся из `ER/Names/GameAreaParam.txt`.
///
/// В таблице лежит НЕ имя, а его id в тех же текстах игры (слот 18), из
/// которых имя босса читает `learn` в бою: тогда на экране оно появляется на
/// языке игрока и ровно в том написании, что рисует сама игра.
///
/// **Боссы DLC сюда входят наравне с ванильными** - их имена лежат отдельным
/// слоем (`NpcName_dlc01.fmg`, слот 328), и `text_layered` его находит. Ждать
/// первой встречи с ними не нужно; проверено сверкой всех 210 строк с
/// выгрузкой `NpcName` + `NpcName_dlc01` (2026-08-30, 103 id из DLC).
///
/// Английская строка остаётся у трёх, где выбор пришлось бы выдумывать:
/// `Ancient Dragon` (Лансеакс или Сенессакс), `Demi-Human Queen` (Марго,
/// Мэгги или Гилика) и `Crucible Illusion` (что это за строка, неизвестно).
/// Прямое решение пользователя: оставить их английскими.
///
/// Строка `NpcParam` для этого не годится - у боссов `name_id` там нулевой
/// (замер 2026-08-27: 12 имён на 160 строк).
const NAME_TABLE: &str = include_str!("boss_names.txt");

/// Разобранная таблица: что делать с флагом - спросить парам или взять как
/// есть.
fn table() -> &'static HashMap<u32, (Result<u32, String>, bool)> {
    static T: std::sync::OnceLock<HashMap<u32, (Result<u32, String>, bool)>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| {
        NAME_TABLE
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .filter_map(|l| {
                let (flag, val) = l.split_once(' ')?;
                let flag = flag.trim().parse().ok()?;
                // Хвостовое `R` - за босса дают воспоминание. Отдельным полем,
                // потому что из парамов это не выводится: лот с наградой не
                // привязан ни к флагу победы, ни к врагу (замер 2026-08-28),
                // его выдаёт событие карты.
                let val = val.trim();
                let (val, rem) = match val.strip_suffix(" R") {
                    Some(head) => (head.trim(), true),
                    None => (val, false),
                };
                Some((
                    flag,
                    (
                        match val.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
                            Some(name) => Err(name.to_string()),
                            None => Ok(val.parse().ok()?),
                        },
                        rem,
                    ),
                ))
            })
            .collect()
    })
}

/// Флаги победы боссов, за которых дают воспоминание.
///
/// Признак хранится в таблице, а не выводится из парамов: лот с наградой не
/// привязан ни к флагу победы, ни к врагу - на него не ссылается ни один
/// `NpcParam`, и самого флага в его строке нет (сырой скан 2026-08-28).
/// Воспоминание выдаёт событие карты, а события лежат в файлах карт, куда мод
/// не лезет. Источник признака - подписи лотов в
/// `ER/Names/ItemLotParam_map.txt` («[Stormveil - Godrick] Remembrance of the
/// Grafted»).
fn remembrance_flags() -> &'static std::collections::HashSet<u32> {
    static SET: std::sync::OnceLock<std::collections::HashSet<u32>> = std::sync::OnceLock::new();
    SET.get_or_init(|| table().iter().filter(|(_, v)| v.1).map(|(f, _)| *f).collect())
}

/// Имя существа так, как его зовёт сама игра.
///
/// Тексты имён адресуются МОДЕЛЬЮ существа:
/// `id = 900000000 + chr * 1000 (+ вариант)`. Проверено на выгрузке
/// `NpcName` из игры 2026-08-27: `905130000` - это `c5130` (Мессмер),
/// `902130000` - `c2130` (Маргит), `904750000` - `c4750` (Годрик).
///
/// Отсюда главное: имя не нужно ниоткуда импортировать - хватает номера
/// модели, а его даёт само существо. Поэтому босс из мода, которого нет ни в
/// одной таблице, получает имя ровно так же, как ванильный, - лишь бы мод
/// завёл ему строку в текстах игры.
///
/// `NpcParam.name_id` остаётся запасным путём: у боссов он нулевой, но у
/// именных NPC (Блайд, торговцы) заполнен, а модель у них общая с рядовыми.
fn chr_name(chr: u32) -> Option<String> {
    (chr > 0).then(|| crate::msg::text_layered(crate::msg::NPC_NAME, name_id_of(chr))).flatten()
}

/// Номер строки в текстах имён по модели существа.
const fn name_id_of(chr: u32) -> i32 {
    (900_000_000 + chr * 1000) as i32
}

/// То же по номеру строки `NpcParam`: модель - его старшие разряды.
fn npc_name(repo: &SoloParamRepository, npc: u32) -> Option<String> {
    chr_name(npc / 10000).or_else(|| {
        let row = repo.get::<NpcParam>(npc)?;
        crate::msg::text_layered(crate::msg::NPC_NAME, row.name_id())
            .or_else(|| crate::msg::text_layered(crate::msg::NPC_NAME, row.role_name_id()))
    })
}

/// Имя босса по флагу победы, из таблицы.
///
/// Строка парама даёт локализованный текст, готовое имя - английский запасной
/// вариант. Мод с новыми боссами в таблицу не попадает и остаётся без имени -
/// его подставит `learn`, поймав имя в бою.
fn table_name(_repo: &SoloParamRepository, flag: u32) -> Option<String> {
    match &table().get(&flag)?.0 {
        // Слоями, а не одним слотом: имена Земель Теней лежат отдельным
        // слоем поверх базового, и без этого DLC-боссы оставались безымянными
        // (живьём 2026-08-28: таблица знала 197 флагов, а имя давала 161).
        Ok(text_id) => crate::msg::text_layered(crate::msg::NPC_NAME, *text_id as i32),
        Err(name) => Some(name.clone()),
    }
}

/// Имена боссов из существ, загруженных в мир.
///
/// **Связки «флаг победы -> имя» в парамах с версии игры 2.7.0.0 нет.** Она
/// жила в маркерах карты (`WorldMapPointParam.text_disable_flag_id1`), и
/// оттуда её убрали: замер 2026-08-27 нашёл в 472 маркерах ровно два ненулевых
/// флага, а сплошной скан всех 239 парамов игры не дал ни одного, где рядом с
/// флагом победы лежало бы имя.
///
/// Зато у FromSoft есть давнее соглашение: **entity id босса равен его флагу
/// победы** (Годрик - 10000800 и там, и там). Значит имя можно взять у самого
/// существа, как только игра его загрузила: `event_entity_id` -> строка
/// `NpcParam` -> `name_id` -> текст в слоте имён врагов.
///
/// Работает по мере игры: босс подгружается, когда игрок подходит к его арене,
/// - входить в туман не нужно. Выученное уходит в тот же файл, что и имена,
/// пойманные в бою (`learn`), и переживает перезапуск.
fn learn_from_world(repo: &SoloParamRepository, flags: &std::collections::HashSet<u32>) -> usize {
    let Ok(world) = (unsafe { WorldChrMan::instance() }) else {
        return 0;
    };
    let mut learned = LEARNED.lock().unwrap_or_else(|e| e.into_inner());
    let Some(learned) = learned.as_mut() else {
        return 0;
    };
    let mut added = 0usize;
    let open = world.open_field_chr_set.base.characters();
    let rest = world.chr_sets.iter().flatten().flat_map(|s| s.characters());
    for chr in open.chain(rest) {
        let id = chr.event_entity_id;
        if !flags.contains(&id) || learned.contains_key(&id) {
            continue;
        }
        // Модель у существа своя - точнее, чем выводить её из номера строки.
        let Some(name) =
            chr_name(chr.character_id).or_else(|| npc_name(repo, chr.npc_param_id as u32))
        else {
            continue;
        };
        learned.insert(id, name);
        added += 1;
    }
    if added > 0 {
        save_learned(learned);
    }
    added
}

/// Диагностика босс-листа: сколько боссов знает реестр и откуда у них имена.
///
/// Формат разбирает `overlay::draw_debug`: `# ` - заголовок, два пробела -
/// строка секции.
pub fn probe() -> Vec<String> {
    let mut out = vec!["# БОССЫ".into()];
    let Ok(repo) = (unsafe { SoloParamRepository::instance() }) else {
        out.push("  парамы не резолвятся".into());
        return out;
    };
    let flags: std::collections::HashSet<u32> = repo
        .rows::<GameAreaParam>()
        .map(|(_, r)| r.defeat_boss_flag_id())
        .filter(|f| *f != 0)
        .collect();
    // Покрытие таблицы: знает ли она флаг вообще и даёт ли по нему имя.
    let (mut known, mut named) = (0usize, 0usize);
    let mut unknown: Vec<u32> = Vec::new();
    let mut sorted: Vec<u32> = flags.iter().copied().collect();
    sorted.sort_unstable();
    for flag in &sorted {
        match table().get(flag) {
            None => unknown.push(*flag),
            Some(v) => {
                known += 1;
                if match &v.0 {
                    Ok(id) => crate::msg::text_layered(crate::msg::NPC_NAME, *id as i32).is_some(),
                    Err(_) => true,
                } {
                    named += 1;
                }
            }
        }
    }
    out.push(format!("  таблица знает {known}/{} | с именем {named}", flags.len()));
    if !unknown.is_empty() {
        out.push(format!("  без имени: {}", unknown.len()));
        // С картой: у безымянного босса имя ищется в редакторе карт по его
        // entity id, а он равен флагу победы. Без карты это поиск по сотне
        // файлов.
        let maps: HashMap<u32, String> = repo
            .rows::<GameAreaParam>()
            .filter(|(_, r)| r.defeat_boss_flag_id() != 0)
            .map(|(_, r)| {
                (
                    r.defeat_boss_flag_id(),
                    format!(
                        "m{}_{:02}_{:02}",
                        r.boss_map_area_no(),
                        r.boss_map_block_no(),
                        r.boss_map_map_no()
                    ),
                )
            })
            .collect();
        for chunk in unknown.chunks(2) {
            let line: Vec<String> = chunk
                .iter()
                .map(|f| format!("{f} {}", maps.get(f).map_or("?", String::as_str)))
                .collect();
            out.push(format!("  {}", line.join(" | ")));
        }
    }

    out.push(format!("  с воспоминанием: {}", remembrance_flags().len()));
    let mut all = ALL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(reg) = registry(&mut all) else {
        out.push("  реестр не строится".into());
        return out;
    };
    out.push(format!(
        "  в реестре {} | с именем {} | с местом {}",
        reg.bosses.len(),
        reg.bosses.iter().filter(|b| b.name.is_some()).count(),
        reg.bosses.iter().filter(|b| b.place.is_some()).count()
    ));

    // Кто рядом и ПО КАКОМУ ФЛАГУ мод про него судит. Это и есть ответ на
    // «убил, а счётчик не двинулся»: строка называет флаг и его состояние.
    let (Ok(efm), Some(me)) = (unsafe { CSEventFlagMan::instance() }, player_at()) else {
        return out;
    };
    let mut near: Vec<(f32, &Boss)> = reg
        .bosses
        .iter()
        .filter_map(|b| distance(me.0, me.1, b.map, b.pos).map(|(d, _)| (d, b)))
        .collect();
    near.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    for (d, b) in near.iter().take(6) {
        out.push(format!(
            "  {} {} {d:.0}м {}",
            b.flag,
            if efm.virtual_memory_flag.get_flag(b.flag) { "убит" } else { "жив" },
            b.name.as_deref().unwrap_or("?")
        ));
    }
    // Что игра подняла в том же блоке флагов у ближайшего ЖИВОГО. Если
    // убийство ставит не тот флаг, что лежит в параме, он окажется здесь.
    //
    // ponytail: тысяча опросов флага на кадр. Окно диагностики и без того
    // обходит парамы каждый кадр, а живёт оно только под `debug = true`.
    if let Some((_, b)) = near.iter().find(|(_, b)| !efm.virtual_memory_flag.get_flag(b.flag)) {
        let base = b.flag / 1000 * 1000;
        let up: Vec<String> = (base..base + 1000)
            .filter(|f| efm.virtual_memory_flag.get_flag(*f))
            .map(|f| (f % 1000).to_string())
            .take(48)
            .collect();
        out.push(format!("  подняты в {}xxx:", b.flag / 1000));
        for chunk in up.chunks(12) {
            out.push(format!("  {}", chunk.join(" ")));
        }
    }
    out
}

/// Пересчитать расстояния в готовом списке.
///
/// Расстояние меняется с каждым шагом игрока, а всё остальное в строке (имена,
/// флаги, локация, порядок, слияние тёзок) - раз в несколько минут. Поэтому
/// счётчик метров живёт отдельно от `POLL`: одно чтение позиции плюс корень на
/// строку, две сотни строк - это ничто рядом с обходом парамов, который и
/// заставил завести кэш.
///
/// Порядок строк намеренно НЕ пересчитывается: список, который переставляет
/// сам себя под курсором, читать нельзя.
///
/// Нет игрока (загрузка) - оставляем прошлые числа: моргать «-» полсекунды
/// хуже, чем показать метры, устаревшие на эти полсекунды.
fn refresh_distances(rows: &mut [Row]) {
    let Some((cell, pos)) = player_at() else {
        return;
    };
    for r in rows {
        let d = distance(cell, pos, r.map, r.pos);
        (r.dist, r.dy) = (d.map(|x| x.0), d.map(|x| x.1));
    }
}

/// Неубитые боссы: имя, место, расстояние. Список пересобирается не чаще
/// `POLL`, расстояния в нём - на каждый вызов (`refresh_distances`).
///
/// Зовётся только с открытым окном списка и из `nearest` - обход парамов
/// дорогой, а список нужен ровно тогда, когда на него смотрят.
pub fn rows(radius: f32) -> Vec<Row> {
    // Тесты рисуют окно списка настоящим ImGui, а игровых синглтонов в тестовом
    // процессе нет вовсе. Проверяется отрисовка, а не чтение памяти - то же
    // правило, по которому `send_key` в тестах молчит.
    if cfg!(test) {
        return Vec::new();
    }
    let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    // Радиус входит в условие кэша: без этого ползунок в настройках не давал
    // бы результата целую секунду, и его двигали бы вслепую.
    if live.1.is_some_and(|t| t.elapsed() < POLL) && live.2 == radius {
        refresh_distances(&mut live.0);
        return live.0.clone();
    }
    live.1 = Some(Instant::now());
    live.2 = radius;

    let mut all = ALL.lock().unwrap_or_else(|e| e.into_inner());
    // Кому имени ещё не хватает. Заимствование реестра кончается здесь же -
    // ниже он берётся заново, потому что обучение могло его сбросить.
    let want: std::collections::HashSet<u32> = {
        let Some(reg) = registry(&mut all) else {
            return live.0.clone();
        };
        reg.bosses.iter().filter(|b| b.name.is_none()).map(|b| b.flag).collect()
    };
    // Имена берём у самих существ, пока игрок рядом с их аренами: в парамах
    // связки «флаг победы -> имя» с 2.7.0.0 больше нет.
    if !want.is_empty() {
        if let Ok(repo) = unsafe { SoloParamRepository::instance() } {
            if learn_from_world(repo, &want) > 0 {
                *all = None;
            }
        }
    }
    let Some(reg) = registry(&mut all) else {
        return live.0.clone();
    };
    let Ok(efm) = (unsafe { CSEventFlagMan::instance() }) else {
        return live.0.clone();
    };

    // Где стоит игрок. Нет его (загрузка) - список остаётся, просто без
    // расстояний и без «рядом».
    let me = player_at();
    // Регион игрока считается тем же способом, что и у боссов, - иначе «здесь»
    // сравнивало бы разное.
    // Локация игрока считается тем же способом, что и у боссов, - иначе
    // «здесь» сравнивало бы разное.
    let my_place = me.and_then(|(cell, pos)| place_of(cell, pos, &reg.graces));

    let mut rows: Vec<Row> = reg
        .bosses
        .iter()
        .map(|b| {
            let (dist, dy) = me
                .and_then(|(cell, pos)| distance(cell, pos, b.map, b.pos))
                .map_or((None, None), |(d, dy)| (Some(d), Some(dy)));
            Row {
                // Имени может не быть вовсе: с версии игры 2.7.0.0 связка
                // «флаг победы -> маркер карты» из парамов исчезла, и до
                // первой встречи с боссом взять имя неоткуда. Локация и
                // расстояние при этом известны, и это уже полезно - строка
                // говорит «здесь остался босс», а `learn` подставит имя, как
                // только игрок до него дойдёт.
                name: b.name.clone().unwrap_or_else(|| crate::i18n::t("Boss").to_string()),
                place: b
                    .place
                    .clone()
                    .unwrap_or_else(|| format!("m{}_{:02}_{:02}", b.map.0, b.map.1, b.map.2)),
                // «Здесь» - это своя локация ИЛИ просто рядом. Одного
                // совпадения локации мало: их полсотни, и та, в которой стоит
                // игрок, сплошь и рядом не принадлежит ни одному боссу -
                // живьём 2026-08-27 фильтр давал ноль посреди Кэлида.
                here: (my_place.is_some() && my_place == b.place)
                    || dist.is_some_and(|d| reach(d, dy.unwrap_or(0.0)) <= radius),
                dist,
                dy,
                killed: efm.virtual_memory_flag.get_flag(b.flag),
                remembrance: b.remembrance,
                count: 1,
                map: b.map,
                pos: b.pos,
            }
        })
        .collect();
    sort_rows(&mut rows);
    collapse(&mut rows);
    live.0 = rows;
    live.0.clone()
}

/// Схлопывает тёзок одной локации в одну строку со счётчиком.
///
/// Зовётся ПОСЛЕ сортировки: она уже поставила их рядом. Убитый и живой тёзка
/// не сливаются - иначе прогресс по локации был бы виден неверно.
fn collapse(rows: &mut Vec<Row>) {
    let mut out: Vec<Row> = Vec::with_capacity(rows.len());
    for r in rows.drain(..) {
        match out.last_mut() {
            Some(prev) if prev.name == r.name && prev.place == r.place && prev.killed == r.killed => {
                prev.count += 1;
                // Из группы показываем ближайшего: до остальных всё равно
                // дальше, а одно число на строку - это одно число.
                // Из группы показываем ближайшего - вместе с его высотой.
                if r.reach().unwrap_or(f32::MAX) < prev.reach().unwrap_or(f32::MAX) {
                    prev.dist = r.dist;
                    prev.dy = r.dy;
                }
                prev.here |= r.here;
            }
            _ => out.push(r),
        }
    }
    *rows = out;
}

/// Порядок строк на экране: свой регион вперёд, внутри - ближние, дальше -
/// по алфавиту.
///
/// Место - ключ ВЫШЕ имени и расстояния (кроме своего региона), потому что
/// окно рисует заголовок при смене места: разорванная группа дала бы одну и ту
/// же локацию дважды в разных концах списка.
fn coded(place: &str) -> bool {
    place.starts_with('m') && place.split('_').count() == 3
}

fn sort_rows(rows: &mut [Row]) {
    // Близость локации - минимум по её ЖИВЫМ боссам: зачищенной незачем стоять
    // наверху. Считается здесь же, а не хранится в `Row`: иначе его можно
    // забыть выставить, и группа разъедется молча (тест это и поймал).
    let mut nearest: HashMap<String, f32> = HashMap::new();
    for r in rows.iter().filter(|r| !r.killed) {
        let slot = nearest.entry(r.place.clone()).or_insert(f32::MAX);
        *slot = slot.min(r.reach().unwrap_or(f32::MAX));
    }
    let near = |p: &str| nearest.get(p).copied().unwrap_or(f32::MAX);
    let cmp = |x: f32, y: f32| x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal);
    rows.sort_by(|a, b| {
        // Локации без имени (остался m-код) уходят в конец: наверху список
        // должен читаться, а не начинаться с кодов карт.
        coded(&a.place)
            .cmp(&coded(&b.place))
            // Дальше - ЛОКАЦИЯ целиком, и только потом что внутри неё. Раньше
            // здесь стоял `here`, и при включённом радиусе «своими»
            // становились все строки подряд: сортировка уходила в чистое
            // расстояние, а группы рвались - живьём 2026-08-27 «Звёздные
            // пустоши» встретились в списке трижды.
            .then_with(|| cmp(near(&a.place), near(&b.place)))
            .then_with(|| a.place.cmp(&b.place))
            .then_with(|| a.killed.cmp(&b.killed))
            .then_with(|| cmp(a.dist.unwrap_or(f32::MAX), b.dist.unwrap_or(f32::MAX)))
            .then_with(|| a.name.cmp(&b.name))
    });
}

// ---------------------------------------------------------------------------
// Имена, выученные в бою
// ---------------------------------------------------------------------------
//
// Мод, добавляющий босса, обычно заводит ему только строку `GameAreaParam` и
// свой скрипт: ни маркера на карте, ни благодати рядом с ареной у него нет, и
// имени взять неоткуда ни одним из трёх обычных путей. Живьём 2026-08-27
// «Эгхил, крылатый дракон» именно так и не попал в список.
//
// Зато во время боя имя у нас уже есть - его рисует сама игра над полоской, и
// мод его читает давно, ради счётчика попыток. Остаётся связать его со строкой
// парама: игрок в этот момент стоит на арене, а строка знает её координаты, -
// значит ближайшая к игроку неубитая строка и есть этот босс.

/// Файл с выученными именами: `флаг = имя`, рядом с DLL.
const LEARNED_FILE: &str = "game_information_counter.bosses";

/// Дальше этого имя не присваивается: игрок стоит НА арене, а не рядом с ней.
///
/// Не слишком мало: у строки парама координата - середина арены, а у дракона
/// или у полевого босса она большая, и игрок легко оказывается в сотне метров
/// от этой точки, всё ещё сражаясь.
const LEARN_RADIUS: f32 = 150.0;

static LEARNED: Mutex<Option<HashMap<u32, String>>> = Mutex::new(None);
/// Куда писать. Ставится один раз при загрузке мода - `GetModuleFileNameW`
/// требует наш `HMODULE`, а он известен только там.
static LEARNED_PATH: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// Запоминает, где лежит файл выученных имён, и читает его.
pub fn init(dll: usize) {
    let Some(path) = crate::config::dll_sibling(dll, LEARNED_FILE) else {
        return;
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    *LEARNED.lock().unwrap_or_else(|e| e.into_inner()) = Some(parse_learned(&text));
    *LEARNED_PATH.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
}

/// Ближайший живой босс в радиусе - для строки на панели.
///
/// Один, а не список: на панели место дорогое, а «ближайший» отвечает на
/// вопрос «куда идти» лучше, чем перечисление. Берётся из того же кэша, что и
/// окно, поэтому звать можно каждый кадр - и метры в нём каждый кадр свежие.
pub fn nearest(radius: f32) -> Option<(String, f32, f32)> {
    rows(radius)
        .into_iter()
        .filter(|r| !r.killed)
        .filter_map(|r| Some((r.reach()?, r.name, r.dist?, r.dy.unwrap_or(0.0))))
        .filter(|(reach, ..)| *reach <= radius)
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, name, d, dy)| (name, d, dy))
}

/// Забыть все выученные имена.
///
/// Привязка идёт по расстоянию, и рядом стоящие боссы теоретически могут
/// перепутаться. Кнопка возвращает список к тому, что знают парамы, - дешевле,
/// чем правило, которое пытается угадать такой случай само.
pub fn forget_learned() {
    if let Some(map) = LEARNED.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        map.clear();
    }
    if let Some(path) = LEARNED_PATH.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = std::fs::remove_file(path);
    }
    *ALL.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Сколько имён выучено в бою.
pub fn learned_count() -> usize {
    LEARNED.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, HashMap::len)
}

/// Разбор файла выученных имён.
///
/// Битая строка пропускается, а не роняет загрузку: файл лежит рядом с DLL и
/// правится руками, как и остальные наши.
fn parse_learned(text: &str) -> HashMap<u32, String> {
    text.lines()
        .filter_map(|line| {
            let (flag, name) = line.split_once('=')?;
            let name = name.trim();
            (!name.is_empty()).then_some((flag.trim().parse().ok()?, name.to_string()))
        })
        .collect()
}

/// Связывает имя с боссом, рядом с которым сейчас дерётся игрок.
///
/// Зовётся каждый кадр боя, поэтому первым делом дешёвые проверки: без имени,
/// без реестра и без файла делать нечего. Настоящая работа - только когда имя
/// новое.
///
/// **Замки берутся по одному и никогда не вкладываются.** Первая версия
/// держала `LEARNED` через весь вызов, а внутри звала `registry()` -> `build()`,
/// который берёт тот же мьютекс. `std::sync::Mutex` не реентрантный: повторный
/// захват на том же потоке это не паника, а вечное ожидание. Живьём
/// 2026-08-28 (игра 2.7.0.0) это вешало игру намертво при входе в бой с
/// боссом - в дампе 160 потоков, все в ожидании, ни один не крутится.
pub fn learn(names: &[String]) {
    if names.is_empty() {
        return;
    }
    // Файл ещё не прочитан - учить некуда. Замок снимается этой же строкой.
    if LEARNED.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
        return;
    }
    // Точная связка вперёд привязки по расстоянию: она не требует радиуса
    // вовсе. Сработала - имени здесь больше делать нечего.
    if learn_by_flag(&names[0]) {
        return;
    }
    let Some(me) = player_at() else {
        return;
    };

    // Ближайшая к игроку строка парама - и есть арена, на которой он стоит.
    //
    // Имя берётся у ЛЮБОЙ строки, а не только у безымянной: то, что игра
    // рисует над полоской, точнее любого парамового источника. Живьём
    // 2026-08-27 Агил стоял в списке «Крылатым драконом» - обобщённым именем
    // из маркера карты, - хотя игра зовёт его «Эгхил, крылатый дракон».
    //
    // Из реестра забираем ФЛАГ, а не ссылку на босса: заимствование кончается
    // вместе с замком, и дальше он никому не мешает.
    let nearest = {
        let mut all = ALL.lock().unwrap_or_else(|e| e.into_inner());
        let Some(reg) = registry(&mut all) else {
            return;
        };
        reg.bosses
            .iter()
            .filter_map(|b| distance(me.0, me.1, b.map, b.pos).map(|(d, _)| (d, b.flag)))
            .filter(|(d, _)| *d <= LEARN_RADIUS)
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, flag)| flag)
    };
    let Some(flag) = nearest else {
        return;
    };

    // Двойной босс даёт два имени - берём первое: строка парама одна на бой.
    let name = names[0].clone();
    {
        let mut learned = LEARNED.lock().unwrap_or_else(|e| e.into_inner());
        let Some(learned) = learned.as_mut() else {
            return;
        };
        if learned.get(&flag) == Some(&name) {
            return;
        }
        learned.insert(flag, name);
        save_learned(learned);
    }
    // Реестр держит имена внутри себя - пересоберётся со следующим опросом.
    *ALL.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Безымянные боссы, чей флаг победы на начало боя ещё не поднят, когда мы
/// смотрели на них в прошлый раз и выучили ли уже кого-то в этом бою.
static WATCHED: Mutex<(Vec<u32>, Option<Instant>, bool)> =
    Mutex::new((Vec::new(), None, false));

/// Пауза между вызовами, после которой снимок считается протухшим: `learn`
/// зовётся каждый кадр боя, значит перерыв - это конец боя.
const WATCH_GAP: Duration = Duration::from_secs(3);

/// Дальше этого флаг считается чужим. Шире `LEARN_RADIUS` на порядок - точка
/// парама у полевого босса далеко от места боя, - но не «вся карта»: тайл
/// открытого мира это 256 м, то есть четыре тайла вокруг.
const WATCH_RADIUS: f32 = 1000.0;

/// Связывает имя с флагом, который поднялся ПОКА игра рисует это имя.
///
/// Радиуса не требует вовсе, и этим лучше привязки по расстоянию: у полевого
/// босса точка из парама - середина его маршрута, а дерутся с ним где угодно.
/// Живьём 2026-09-07 Ночной всадник так и остался безымянным - убит, флаг
/// поднят, а до точки парама 212 м при `LEARN_RADIUS` 150.
///
/// Ловится это уже после победы: полоска пропадает раньше флага, зато
/// `boss_name` держится весь `BOSS_LINGER`, и `learn` продолжает зваться.
///
/// **В снимок идут только те, кто рядом.** Флаг может подняться где угодно и
/// по любой причине - событие карты, второй бой следом, - и без этого имя
/// босса, с которым игрок дерётся сейчас, уехало бы боссу на другом конце
/// Междуземья. `distance` заодно отсекает другие карты: у неё сравнимы либо
/// два тайла открытого мира, либо одна и та же карта.
///
/// **Замки только по одному.** Тот же фугас, что и в `learn`: `registry()`
/// внутри берёт `LEARNED`, поэтому реестр читается отдельным шагом.
fn learn_by_flag(name: &str) -> bool {
    let Ok(efm) = (unsafe { CSEventFlagMan::instance() }) else {
        return false;
    };
    let stale = {
        let w = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
        w.1.is_none_or(|t| t.elapsed() > WATCH_GAP)
    };
    if stale {
        // Позиции нет (идёт загрузка) - снимок не строим вовсе: пустой он
        // протух бы молча, и весь бой прошёл бы впустую.
        let Some(me) = player_at() else {
            return false;
        };
        let fresh: Vec<u32> = {
            let mut all = ALL.lock().unwrap_or_else(|e| e.into_inner());
            let Some(reg) = registry(&mut all) else {
                return false;
            };
            reg.bosses
                .iter()
                .filter(|b| b.name.is_none() && !efm.virtual_memory_flag.get_flag(b.flag))
                .filter(|b| {
                    distance(me.0, me.1, b.map, b.pos).is_some_and(|(d, _)| d <= WATCH_RADIUS)
                })
                .map(|b| b.flag)
                .collect()
        };
        *WATCHED.lock().unwrap_or_else(|e| e.into_inner()) = (fresh, Some(Instant::now()), false);
        return false;
    }

    let flag = {
        let mut w = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
        w.1 = Some(Instant::now());
        // Этот бой уже назван. Отвечаем «да» до конца боя, иначе привязка по
        // расстоянию повесила бы то же имя ещё и соседней строке в 150 м.
        //
        // ponytail: снимок обнуляет только пауза, поэтому второй безымянный
        // босс, начатый в те же три секунды, по флагу не выучится. Сбросом по
        // смене имени лечится, но случай требует двух безымянных подряд без
        // передышки - таких на всю игру десяток и стоят они по разным углам.
        if w.2 {
            return true;
        }
        let Some(flag) = pick_risen(&mut w.0, |f| efm.virtual_memory_flag.get_flag(f)) else {
            return false;
        };
        w.2 = true;
        flag
    };
    {
        let mut learned = LEARNED.lock().unwrap_or_else(|e| e.into_inner());
        let Some(learned) = learned.as_mut() else {
            return false;
        };
        learned.insert(flag, name.to_string());
        save_learned(learned);
    }
    *ALL.lock().unwrap_or_else(|e| e.into_inner()) = None;
    true
}

/// Кто из наблюдаемых поднялся - и снимок гасится целиком.
///
/// Целиком, а не одной записью: один бой - одно имя. Поднимись за кадр два
/// флага, второй получил бы то же самое имя на следующем.
fn pick_risen(watch: &mut Vec<u32>, up: impl Fn(u32) -> bool) -> Option<u32> {
    let flag = watch.iter().copied().find(|f| up(*f))?;
    watch.clear();
    Some(flag)
}

/// Файл выученных имён: `флаг = имя`, по строке на босса.
fn save_learned(learned: &HashMap<u32, String>) {
    let text: String = {
        let mut lines: Vec<String> = learned.iter().map(|(f, n)| format!("{f} = {n}")).collect();
        lines.sort();
        lines.join("
")
    };
    if let Some(path) = LEARNED_PATH.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        crate::config::write_atomic(path, &text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ближе тот, до кого ближе ДОБИРАТЬСЯ. Живьём босс в километре под
    /// землёй обходил соседа на своём уровне: по горизонтали он был ближе.
    #[test]
    fn height_counts_when_picking_the_nearest() {
        assert!(reach(50.0, 1000.0) > reach(300.0, 0.0), "километр вниз дальше трёхсот метров по прямой");
        assert_eq!(reach(30.0, 40.0), 50.0, "обычная гипотенуза");
        assert_eq!(reach(120.0, 0.0), 120.0, "на своём уровне - те же метры");
    }

    /// Один бой даёт одно имя: поднявшихся за кадр может быть двое, и второй
    /// не должен получить имя первого на следующем кадре.
    #[test]
    fn a_fight_names_exactly_one_boss() {
        let mut watch = vec![10, 20, 30];
        assert_eq!(pick_risen(&mut watch, |f| f == 20 || f == 30), Some(20));
        assert!(watch.is_empty(), "снимок гасится целиком");
        assert_eq!(pick_risen(&mut watch, |_| true), None, "гасить больше нечего");

        let mut watch = vec![10, 20];
        assert_eq!(pick_risen(&mut watch, |_| false), None);
        assert_eq!(watch.len(), 2, "никто не поднялся - ждём дальше");
    }

    /// Имя адресуется моделью существа. Числа сверены с выгрузкой `NpcName`
    /// из игры: на них стоит и таблица, и распознавание боссов из модов.
    #[test]
    fn name_id_follows_the_model() {
        assert_eq!(name_id_of(2130), 902_130_000, "Маргит, c2130");
        assert_eq!(name_id_of(4750), 904_750_000, "Годрик, c4750");
        assert_eq!(name_id_of(5130), 905_130_000, "Мессмер, c5130");
    }

    /// Таблица имён разбирается в обе формы: номер строки парама и готовое имя.
    /// Ключ - флаг победы, он же id строки `GameAreaParam`.
    #[test]
    fn name_table_parses_both_forms() {
        let t = table();
        assert!(t.len() > 200, "боссов в таблице {}", t.len());
        // Paramdex не подписал строку NpcParam - имя лежит текстом.
        // Id имени известен - текст возьмёт игра, на языке игрока. У Годрика
        // рядом стоит и признак воспоминания.
        assert!(matches!(t.get(&10000800), Some((Ok(id), true)) if *id > 0));
        assert!(matches!(t.get(&10000850), Some((Ok(_), false))));
        // А здесь id ещё неизвестен, и имя лежит текстом.
        assert!(t.values().any(|v| v.0.is_err()), "английский запасной вариант должен быть");
        // Воспоминаний в игре 25, боссов - 24: у Радагона и Элден Бист он
        // общий, это один флаг победы.
        assert_eq!(remembrance_flags().len(), 24, "боссов с воспоминанием");
        assert!(t.get(&1).is_none(), "мусорный ключ не должен разбираться");
    }

    fn grace(cell: Cell, x: f32, z: f32, place: &str) -> Grace {
        Grace { cell, pos: (x, 0.0, z), place: place.into() }
    }

    fn graces() -> Vec<Grace> {
        vec![
            grace((10, 0, 0), 0.0, 0.0, "Замок Штормвейл"),
            grace((10, 0, 0), 200.0, 0.0, "Туннель под замком"),
            grace((60, 40, 50), 0.0, 0.0, "Лимгрейв"),
            grace((60, 43, 37), 0.0, 0.0, "Кэлид"),
        ]
    }

    /// Внутри одной карты локация берётся у БЛИЖАЙШЕЙ благодати, а не у первой
    /// попавшейся: у замка и у туннеля под ним клетка общая.
    #[test]
    fn the_nearest_grace_of_the_same_map_names_the_place() {
        let g = graces();
        assert_eq!(place_of((10, 0, 0), (10.0, 0.0, 0.0), &g).as_deref(), Some("Замок Штормвейл"));
        assert_eq!(place_of((10, 0, 0), (190.0, 0.0, 0.0), &g).as_deref(), Some("Туннель под замком"));
    }

    /// Глухой угол оверворлда своей благодати не имеет - берём ближайшую по
    /// расстоянию, но НЕ из чужой зоны: там «рядом» не значит ничего.
    #[test]
    fn a_cell_without_a_grace_borrows_from_its_neighbour() {
        let g = graces();
        assert_eq!(place_of((60, 41, 50), (0.0, 0.0, 0.0), &g).as_deref(), Some("Лимгрейв"));
        assert_eq!(place_of((60, 43, 38), (0.0, 0.0, 0.0), &g).as_deref(), Some("Кэлид"));
        assert_eq!(place_of((99, 0, 0), (0.0, 0.0, 0.0), &g), None, "чужая зона не подходит");
    }

    /// Высота идёт отдельным числом и считается там же, где расстояние: в
    /// своей карте и через тайлы одной зоны. У чужой карты координаты
    /// локальные - там не сравнимо ничего.
    #[test]
    fn height_goes_separately_from_the_distance() {
        let me = ((60u8, 43u8, 37u8), (0.0, 1.0, 0.0));
        let same = distance(me.0, me.1, (60, 43, 37), (10.0, 31.0, 0.0));
        assert_eq!(same, Some((10.0, 30.0)), "по горизонтали 10 м, вверх 30");

        let other = distance(me.0, me.1, (60, 44, 37), (10.0, 91.0, 0.0));
        let (d, dy) = other.expect("соседний тайл той же зоны сравним");
        assert!(d > 200.0, "расстояние через тайл считается");
        assert_eq!(dy, 90.0);

        assert_eq!(distance(me.0, me.1, (10, 0, 0), (0.0, 0.0, 0.0)), None, "чужая карта");
    }

    /// В открытом мире решает расстояние, а не номер клетки: босс у восточного
    /// края тайла ближе к благодати СЛЕДУЮЩЕГО тайла, чем к своей собственной
    /// на другом его конце. Сравнение номеров отвечало здесь неверно - живьём
    /// это увело Годрика из Лимгрейва на «Тракт Беллума».
    #[test]
    fn across_tiles_distance_wins_over_the_cell_number() {
        let g = vec![
            grace((60, 39, 50), 0.0, 0.0, "своя клетка, но далеко"),
            grace((60, 40, 50), 5.0, 0.0, "соседняя клетка, но рядом"),
        ];
        let at_the_edge = (250.0, 0.0, 0.0);
        assert_eq!(
            place_of((60, 39, 50), at_the_edge, &g).as_deref(),
            Some("соседняя клетка, но рядом")
        );
    }

    /// У отдельных карт координаты локальные, поэтому РАССТОЯНИЕ до чужой
    /// карты не значит ничего. Но если своей благодати нет вовсе, соседняя
    /// карта той же зоны - обычно и есть нужное место, и это лучше m-кода.
    #[test]
    fn a_map_without_a_grace_falls_back_to_its_zone() {
        let g = vec![
            grace((10, 0, 0), 999.0, 999.0, "Замок Штормвейл"),
            grace((12, 8, 0), 0.0, 0.0, "Нокрон"),
        ];
        // Координаты благодати далеко, но зона та же - место всё равно её.
        assert_eq!(place_of((10, 1, 0), (0.0, 0.0, 0.0), &g).as_deref(), Some("Замок Штормвейл"));
        assert_eq!(place_of((12, 9, 0), (0.0, 0.0, 0.0), &g).as_deref(), Some("Нокрон"));
        assert_eq!(place_of((44, 0, 0), (0.0, 0.0, 0.0), &g), None, "чужая зона - не место");
    }

    /// А в открытом мире запасной путь не нужен и вреден: зона там одна на
    /// пол-игры, и «соседняя карта» означала бы любую точку Междуземья.
    #[test]
    fn the_open_world_has_no_zone_fallback() {
        let g = vec![grace((61, 40, 40), 0.0, 0.0, "Земли Теней")];
        assert_eq!(place_of((60, 39, 50), (0.0, 0.0, 0.0), &g), None);
    }

    /// Имя с `=` внутри ломало бы разбор, если бы делили с конца, а пустое -
    /// это «имени нет», а не босс без названия.
    #[test]
    fn the_learned_file_survives_hand_editing() {
        let m = parse_learned(
            "10010800 = Эгхил, крылатый дракон

мусор
25000800=Матерь Пальцев
30 = 
40 = a = b
",
        );
        assert_eq!(m.get(&10010800).map(String::as_str), Some("Эгхил, крылатый дракон"));
        assert_eq!(m.get(&25000800).map(String::as_str), Some("Матерь Пальцев"));
        assert_eq!(m.get(&30), None, "пустое имя - это не имя");
        assert_eq!(m.get(&40).map(String::as_str), Some("a = b"));
        assert_eq!(m.len(), 3);
    }

    fn boss(flag: u32, map: Cell, place: &str) -> Boss {
        Boss {
            flag,
            name: Some("Годрик".into()),
            map,
            pos: (0.0, 0.0, 0.0),
            place: Some(place.into()),
            remembrance: false,
        }
    }

    /// Годрик описан двумя строками - в замке и на тайле Лимгрейва рядом с
    /// ним. В списке он должен быть один, и именно в замке.
    #[test]
    fn one_flag_means_one_boss() {
        let rows = [
            boss(10000800, (60, 39, 50), "Тракт Беллума"),
            boss(10000800, (10, 0, 0), "Замок Грозовой Завесы"),
            boss(25000800, (25, 0, 0), "Река Сиофра"),
        ];
        let out = rows.into_iter().fold(Vec::new(), one_per_flag);
        assert_eq!(out.len(), 2, "две строки одного флага - это один босс");
        assert_eq!(out[0].place.as_deref(), Some("Замок Грозовой Завесы"), "отдельная карта важнее тайла");
    }

    /// Порядок строк в параме не наш, поэтому правило обязано работать и
    /// наоборот - когда карта пришла первой.
    #[test]
    fn the_map_wins_whichever_row_came_first() {
        let rows = [
            boss(10000800, (10, 0, 0), "Замок Грозовой Завесы"),
            boss(10000800, (60, 39, 50), "Тракт Беллума"),
        ];
        let out = rows.into_iter().fold(Vec::new(), one_per_flag);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].place.as_deref(), Some("Замок Грозовой Завесы"));
    }

    fn row(name: &str, place: &str, dist: Option<f32>, here: bool) -> Row {
        Row {
            remembrance: false,
            name: name.into(),
            place: place.into(),
            dist,
            here,
            killed: false,
            count: 1,
            dy: None,
            map: (60, 43, 37),
            pos: (0.0, 0.0, 0.0),
        }
    }

    /// Тёзки одной локации сливаются в строку со счётчиком, а убитый с живым -
    /// никогда: иначе прогресс по локации показывал бы неправду.
    #[test]
    fn namesakes_of_one_place_collapse_but_the_dead_stay_apart() {
        let mut rows = vec![
            row("Кристалиец", "Плато Альтус", Some(90.0), false),
            row("Кристалиец", "Плато Альтус", Some(40.0), false),
            Row { killed: true, ..row("Кристалиец", "Плато Альтус", None, false) },
            row("Кристалиец", "Кэлид", None, false),
        ];
        collapse(&mut rows);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].count, 2, "два живых тёзки - одна строка");
        assert_eq!(rows[0].dist, Some(40.0), "показываем ближайшего из группы");
        assert!(rows[1].killed && rows[1].count == 1, "убитый отдельно");
        assert_eq!(rows[2].place, "Кэлид", "другая локация не сливается");
    }

    /// Карта берётся из entity id только у отдельных карт: в открытом мире он
    /// устроен иначе, а клетка там и так верна.
    #[test]
    fn a_dungeon_map_comes_out_of_the_entity_id() {
        assert_eq!(cell_of_entity(10001950, 10), Some((10, 0, 0)));
        // Ради этого случая всё и делалось: благодать катакомб игра приписала
        // к зоне 30, а к какой именно карте - знает только entity id.
        assert_eq!(cell_of_entity(30170800, 30), Some((30, 17, 0)));
        // Ради этого - вторая половина правила: благодать катакомб игра
        // приписала к клетке ВХОДА в открытом мире, и только entity id знает,
        // куда она ведёт.
        assert_eq!(cell_of_entity(30170800, 60), Some((30, 17, 0)));
        assert_eq!(cell_of_entity(1042380800, 60), None, "у открытого мира id длиннее");
        assert_eq!(cell_of_entity(4001950, 10), None, "зона вне диапазона карт - мусор");
        assert_eq!(cell_of_entity(4001950, 60), None, "и от клетки входа тоже");
        assert_eq!(cell_of_entity(0, 10), None);
    }

    /// Тёзки в разных местах - разные боссы, и они обязаны остаться в своих
    /// локациях, а не слипнуться в одну группу (прямой запрос 2026-08-27).
    #[test]
    fn namesakes_stay_in_their_own_regions() {
        let mut rows = vec![
            row("Ночной всадник", "Кэлид", None, false),
            row("Годрик", "Лимгрейв", None, false),
            row("Ночной всадник", "Лимгрейв", None, false),
        ];
        sort_rows(&mut rows);
        let by_place: Vec<(&str, &str)> =
            rows.iter().map(|r| (r.place.as_str(), r.name.as_str())).collect();
        assert_eq!(
            by_place,
            [
                ("Кэлид", "Ночной всадник"),
                ("Лимгрейв", "Годрик"),
                ("Лимгрейв", "Ночной всадник")
            ]
        );
    }

    /// Локация без имени - это m-код карты, и наверху списка он читается
    /// хуже всего. Такие уходят в конец, но своей группой.
    #[test]
    fn coded_places_sink_to_the_bottom() {
        let mut rows = vec![
            row("безымянное место", "m30_17_00", None, false),
            row("Реннала", "Академия Райи Лукарии", None, false),
            row("ещё одно", "m30_17_00", None, false),
            row("Годрик", "Замок Грозовой Завесы", None, false),
        ];
        sort_rows(&mut rows);
        let places: Vec<&str> = rows.iter().map(|r| r.place.as_str()).collect();
        assert_eq!(
            places,
            ["Академия Райи Лукарии", "Замок Грозовой Завесы", "m30_17_00", "m30_17_00"]
        );
    }

    /// Окно печатает заголовок при СМЕНЕ места, поэтому одна локация не имеет
    /// права появиться дважды: иначе «Лимгрейв (3)» встретится в двух местах
    /// списка с разными боссами.
    #[test]
    fn a_region_never_appears_twice() {
        let mut rows = vec![
            row("дальний", "Кэлид", None, false),
            row("свой ближний", "Лимгрейв", Some(10.0), true),
            row("другой дальний", "Кэлид", None, false),
            row("свой дальний", "Лимгрейв", None, true),
        ];
        sort_rows(&mut rows);
        let mut seen: Vec<&str> = Vec::new();
        for r in &rows {
            if seen.last() != Some(&r.place.as_str()) {
                assert!(!seen.contains(&r.place.as_str()), "локация {} разорвана", r.place);
                seen.push(&r.place);
            }
        }
        assert_eq!(rows[0].name, "свой ближний", "ближняя локация идёт первой");
        assert_eq!(rows[1].place, "Лимгрейв", "и её группа не разорвана");
    }
}





