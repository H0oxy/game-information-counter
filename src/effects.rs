//! Эффекты за баллы: правка состояния игрока и его цели.
//!
//! Вторая после спавна мутация игровой памяти, но куда более скромная: только
//! типизированные поля крейта, ни одного сырого оффсета, ни одного детура и ни
//! одного вызова игровой функции.
//!
//! Файл устроен как `spawn.rs`: сверху куратор-таблица и чистые функции,
//! снизу `EffectState`, которому нужен настоящий `&mut` в память игры и
//! который тестами не покрыт.
//!
//! Два вида эффектов, различает их `default_secs`:
//!
//! - **разовые** (лечение, подкрутить босса) - применились и забылись;
//! - **временные** (скорость, выносливость, камера, гравитация, фляги) - живут
//!   в `active`, ПЕРЕПИСЫВАЮТСЯ КАЖДЫЙ КАДР и откатываются по истечении срока.
//!
//! Каждый кадр, а не один раз при покупке: игра пересчитывает и максимум
//! выносливости, и обзор камеры, и наша разовая запись слетела бы на первом же
//! пересчёте.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eldenring::cs::{
    CSBulletManager, CSChrDataModule, CSFeManImp, CSFlipper, FieldInsHandle, FieldInsType,
    ItemCategory, LockCamParam, PlayerGameData, PlayerIns, SoloParamRepository, WorldChrMan,
};
use fromsoftware_shared::FromStatic;

/// Одна строка куратор-списка.
pub struct EffectEntry {
    pub key: &'static str,
    /// Английское название, оно же ключ перевода (см. `i18n`).
    pub label: &'static str,
    /// Сколько длится по умолчанию. `0` - разовый эффект, срока у него нет
    /// вовсе, и ползунок «Длится» такой награде не рисуется.
    pub default_secs: u16,
    /// Одной строкой, ЧТО эффект делает с игроком - не как он устроен. Пусто
    /// там, где название и так всё сказало («Полное лечение» пояснять нечем).
    pub about: &'static str,
    /// Мешает играть. Отсюда считается «Смерти от зрителей»: покупка помехи и
    /// смерть через несколько секунд после неё - зачёт зрителю. Лечение и
    /// фляги смерти не причина, а рунический обмен ничего не портит - он
    /// только считает.
    pub harmful: bool,
}

const fn e(
    key: &'static str,
    label: &'static str,
    default_secs: u16,
    about: &'static str,
    harmful: bool,
) -> EffectEntry {
    EffectEntry { key, label, default_secs, about, harmful }
}

/// Что можно купить. Ключи попадают в файл наград - менять их нельзя, у
/// пользователей уже настроены награды.
pub const EFFECT_TABLE: &[EffectEntry] = &[
    // Разовые, в пользу стримера
    e("heal", "Full heal", 0, "", false),
    e("restore_fp", "Full FP", 0, "", false),
    e("flask_gift", "Plus 3 flask charges", 0, "Tops up 3 flask charges, never above your maximum.", false),
    // Разовые, против стримера
    e("one_hp", "Down to 1 HP", 0, "Health drops to one. Good luck.", true),
    e("boss_heal", "Heal the boss", 0, "The boss gets all its health back. Start over.", true),
    e("boss_hp", "Boss gets 4x HP", 0, "The boss's health bar gets four times longer.", true),
    // Временные
    e("stamina_half", "Half stamina", 180, "Half as many swings and rolls in a row.", true),
    e("stamina_none", "No stamina", 180, "No stamina at all: no sprint, no block, no combos.", true),
    e("slow", "Slow motion", 100, "Everything moves in slow motion, you included.", true),
    e("fast", "Fast forward", 100, "The game runs faster, and so must you.", true),
    e("camera_sway", "Camera sway", 120, "The field of view breathes in and out; aiming gets harder.", true),
    e("flask_lock", "Flasks locked", 90, "Flasks run dry: no healing until it wears off.", true),
    e("rune_trade", "Rune exchange", 180, "Deal damage and earn runes, take damage and lose them - losses cost more.", false),
];


pub fn entry(key: &str) -> Option<&'static EffectEntry> {
    EFFECT_TABLE.iter().find(|e| e.key == key)
}

/// То же самое, но отдаёт `&'static str` ключа: нужен `decode()` в
/// `rewards.rs`, чтобы получить владеющий кусок для `Copy`-варианта
/// `Action::Effect`, не аллоцируя `String`.
pub fn key_of(name: &str) -> Option<&'static str> {
    entry(name).map(|e| e.key)
}

pub fn label(key: &str) -> String {
    match entry(key) {
        Some(e) => crate::i18n::t(e.label).to_string(),
        None => key.to_string(),
    }
}

/// Что эффект делает, одной строкой. Пусто - название говорит само за себя.
pub fn about(key: &str) -> &'static str {
    entry(key).map_or("", |e| crate::i18n::t(e.about))
}

/// Мешает ли эффект играть - см. `EffectEntry::harmful`.
pub fn harmful(key: &str) -> bool {
    entry(key).is_some_and(|e| e.harmful)
}

/// Сколько длится по умолчанию. `0` - разовый.
pub fn default_secs(key: &str) -> u16 {
    entry(key).map_or(0, |e| e.default_secs)
}

/// Потолок длительности. Не вкусовщина: пока эффект жив, мод переписывает поле
/// игры каждый кадр, и падение игры в этот момент - единственное, чего нам не
/// откатить. Чем короче окно, тем меньше цена такого совпадения (тот же довод,
/// что у `MAX_HOLD_MS` в `actions.rs`).
pub const MAX_SECS: u16 = 600;

// ---------------------------------------------------------------------------
// Статусы
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Числа эффектов
// ---------------------------------------------------------------------------

/// Насколько крупно гуляет обзор камеры и за сколько секунд полный
/// вдох-выдох.
const SWAY_AMPLITUDE: f32 = 0.4;
const SWAY_PERIOD_SECS: f32 = 3.0;

const SLOW_SPEED: f32 = 0.5;
const FAST_SPEED: f32 = 1.5;

/// Фляги - обычные предметы инвентаря, и число зарядов лежит у них в
/// `quantity`. Диапазоны id взяты из `ER/Names/EquipParamGoods.txt`: красная
/// `1000..1025`, синяя `1050..1075` (базовая, +1 ... +12, по два id на уровень).
const FLASK_HP: std::ops::RangeInclusive<u32> = 1000..=1025;
const FLASK_FP: std::ops::RangeInclusive<u32> = 1050..=1075;

/// Сколько зарядов доливает «Плюс 3 фляги». Потолок берётся у самой игры -
/// `PlayerGameData::max_hp_flask` / `max_fp_flask`.
const FLASK_GIFT: u32 = 3;

/// «Рунический обмен»: доля нанесённого урона, начисляемая рунами, и доля
/// полученного, списываемая с них. Числа заданы прямым запросом.
const RUNE_GAIN: f32 = 0.6;
const RUNE_LOSS: f32 = 0.8;

/// Насколько крупными порциями двигаются руны. Не косметика: покадровое
/// начисление превращало счётчик в мельтешение, а по нему стример и следит за
/// эффектом. Копим и отдаём разом (прямой запрос 2026-08-24).
const RUNE_BATCH: Duration = Duration::from_secs(3);

/// Пауза между вызовами `poll`, после которой прошлые HP считаются
/// протухшими.
///
/// Порт `elden::RESYNC_GAP` вместе с его причиной: между двумя вызовами могла
/// пройти загрузка, и тогда выгрузка врага читается как «урон во всю полоску»,
/// а перезаход - как воскрешение. Кадровая метка этого не ловит, счётчик
/// кадров двигаем мы сами - нужны настоящие часы.
const RESYNC_GAP: Duration = Duration::from_millis(400);

/// Сколько помним, чей это был снаряд, после того как он пропал из мира.
///
/// К моменту, когда мы видим упавшие HP, стрелы уже нет - игра уничтожает её
/// при попадании, а `last_hit_by` на неё всё ещё указывает. Поэтому владельцев
/// запоминаем ПОКА СНАРЯД ЛЕТИТ и держим ещё несколько секунд.
const BULLET_MEMORY: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Отказы
// ---------------------------------------------------------------------------

pub enum Rejected {
    /// Такого ключа нет в таблице - строку в файле наград правили руками.
    Unknown,
    NoPlayer,
    /// «Подлечить босса» без босса на экране.
    NoBoss,
    /// Парамы ещё не загружены - камеру крутить не из чего.
    NoParams,
}

impl Rejected {
    /// Почему не получилось - человеку в ту же строку окна, где причины отказа
    /// спавна. Лога в моде нет, и без неё кнопка «Проверить» молчит.
    pub fn reason(&self) -> String {
        use crate::i18n::t;
        match self {
            Rejected::Unknown => {
                t("No such effect in the list.").into()
            }
            Rejected::NoPlayer => {
                t("The player is not in the world yet.").into()
            }
            Rejected::NoBoss => {
                t("No boss on screen - no target.").into()
            }
            Rejected::NoParams => {
                t("The game data is still loading.##effects").into()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Состояние
// ---------------------------------------------------------------------------

/// Что вернуть на место, когда время выйдет.
///
/// Вариант на механизм, а не на эффект: «замедлить» и «ускорить» откатываются
/// одинаково, две ступени выносливости - тоже.
enum Restore {
    /// Скорость игры откатывается в единицу, запоминать нечего.
    Speed,
    /// Максимум выносливости, каким он был до эффекта.
    ///
    /// ponytail: снимок, а не пересчёт. Уровень или снаряжение, поменявшиеся
    /// посреди эффекта, откатятся к прежнему максимуму - до следующего
    /// пересчёта самой игрой. Мельче, чем ради этого следить за источником
    /// числа.
    Stamina(i32),
    /// Исходный `cam_fov_y` каждой строки `LockCamParam`: `(id строки, обзор)`.
    ///
    /// **Крутим ПАРАМ, а не саму камеру** - двумя заходами до этого выяснено,
    /// почему. Короче: `CSCamera::pers_cam_1.fov` - результат,
    /// который движок пересчитывает каждый кадр, и запись на `Present`
    /// приходит после отрисовки, то есть впустую. `LockCamParam` - вход, из
    /// которого он этот результат считает, и правка там доезжает до экрана
    /// сама. Тем же местом пользуются и CE-таблицы, и обычные FOV-моды, только
    /// офлайн, через `regulation.bin`.
    ///
    /// Правятся ВСЕ строки разом: какая из них сейчас активна, решает игра
    /// (обычная камера, лок-он, разговор, арена), и угадывать это незачем -
    /// пусть дышит всё.
    Fov(Vec<(u32, f32)>),
    /// «Рунический обмен». Возвращать нечего - руны остаются у игрока, - но
    /// состояние между кадрами нужно: слежка за HP, накопленный остаток и
    /// когда отдавали в прошлый раз.
    Runes {
        watch: DamageWatch,
        pending: f32,
        net: i64,
        /// Сколько всего нанесено и получено за эффект. Только для записки в
        /// окно: без разбивки «руны не начисляются» и «начисляются, но их
        /// съедает списание» с экрана не отличить (жалоба 2026-08-24).
        gross: (f32, f32),
        flushed: Instant,
    },
    /// Сколько было во флягах: заряды `(param_id, заряды)` и счётчики групп
    /// `(pot_group, сколько)`. Второе - из-за того, что обнулённая `quantity`
    /// убирает иконку, но одну флягу выпить всё равно давало.
    Flasks { qty: Vec<(u32, u32)>, pots: Vec<(usize, u32)> },
}

/// Каким механизмом эффект держится. Два эффекта одной группы одновременно
/// висеть не могут, и это не удобство, а корректность: `stamina_none` поверх
/// `stamina_half` запомнил бы УЖЕ УПОЛОВИНЕННЫЙ максимум как «нормальный», и
/// откат оставил бы половину выносливости навсегда.
///
/// Побочно это ограничивает `active` четырьмя записями, то есть и число
/// резолвов синглтона за кадр в `run`.
fn group(key: &str) -> &'static str {
    match key {
        "slow" | "fast" => "speed",
        "stamina_half" | "stamina_none" => "stamina",
        "camera_sway" => "fov",
        "flask_lock" => "flask",
        "rune_trade" => "runes",
        _ => "",
    }
}

/// Переживает ли эффект смерть игрока.
///
/// Смерть снимает всё (прямой запрос 2026-08-24) - кроме рунического обмена
/// (уточнение того же дня). Разница по сути: остальные эффекты МЕШАЮТ играть, и
/// тащить помеху через смерть незачем, а обмен ничего не портит - он считает
/// итог боя. Оборвать его смертью значило бы, что зритель платит за три минуты,
/// а получает первые двадцать секунд, причём ровно в тот момент, когда самое
/// интересное только началось.
fn survives_death(key: &str) -> bool {
    key == "rune_trade"
}

/// Почему снимаем эффекты не по времени.
#[derive(Clone, Copy, PartialEq)]
enum Force {
    /// Обычный кадр: истекает тот, чьё время вышло.
    None,
    /// Снять всё - аварийный выключатель и кнопка «Снять эффекты».
    All,
    /// Смерть игрока: всё, кроме тех, кого щадит `survives_death`.
    ExceptSurvivors,
}

struct Active {
    key: &'static str,
    /// Чем расплатились. Нужен, чтобы снять карточку покупки, если эффект
    /// оборвали досрочно: отсчёт до конца на ней иначе досчитывал бы до нуля
    /// эффекта, которого уже нет.
    redemption_id: String,
    started: Instant,
    until: Instant,
    restore: Restore,
}

#[derive(Default)]
pub struct EffectState {
    active: Vec<Active>,
    /// Чем кончилась последняя покупка - строкой в окно настроек, туда же,
    /// куда пишет `spawn::SpawnState::note`. Единственный канал: лога нет, а
    /// «какой статус выпал» иначе видно только по экрану, и то не всегда.
    note: Option<String>,
}

impl EffectState {
    /// Сколько эффектов сейчас висит. Единственный признак, что мод вообще
    /// что-то делает с игрой: лога нет, а «почему всё медленно» спрашивать
    /// больше не у кого.
    pub fn count(&self) -> usize {
        self.active.len()
    }

    /// Забрать строку о последней покупке. Забирается один раз - висеть до
    /// следующей она не должна.
    pub fn take_note(&mut self) -> Option<String> {
        self.note.take()
    }

    /// Применить эффект. `secs = 0` - взять срок из таблицы.
    pub fn apply(&mut self, key: &str, secs: u16, redemption_id: &str) -> Result<(), Rejected> {
        let entry = entry(key).ok_or(Rejected::Unknown)?;
        if entry.default_secs == 0 {
            return self.apply_once(entry.key);
        }
        let secs = if secs == 0 { entry.default_secs } else { secs };
        let until = Instant::now() + Duration::from_secs(u64::from(secs.min(MAX_SECS)));
        // Уже есть эффект той же группы - забираем его запись себе вместе с
        // её `Restore`: там лежит ИСХОДНОЕ значение, снятое до всех правок.
        // Заводить вторую запись нельзя (см. `group`).
        //
        // Срок только вперёд: короткая покупка не обрывает длинную - то же
        // правило, что у удержания клавиши. Известный потолок: купленный на
        // 10 секунд эффект поверх идущих 180 проживёт все 180.
        if let Some(a) = self.active.iter_mut().find(|a| group(a.key) == group(entry.key)) {
            a.key = entry.key;
            a.until = a.until.max(until);
            // Карточка теперь висит за эту покупку: прежняя досчитает свой
            // отсчёт и уйдёт сама, а обрывать её нечем - эффект-то идёт.
            a.redemption_id = redemption_id.to_string();
            return Ok(());
        }
        let restore = start_timed(entry.key)?;
        // Сколько строк камеры взяли: единственный способ увидеть, что парам
        // вообще нашёлся. Лога в моде нет.
        if let Restore::Fov(base) = &restore {
            self.note = Some(format!("{}: {}", crate::i18n::t("camera rows"), base.len()));
        }
        self.active.push(Active {
            key: entry.key,
            redemption_id: redemption_id.to_string(),
            started: Instant::now(),
            until,
            restore,
        });
        Ok(())
    }

    /// Каждый кадр: держим временные эффекты и снимаем истёкшие.
    ///
    /// `count_damage = false` в меню и катсцене - см. `DamageWatch::poll`:
    /// счёт урона отключается, а не всё слежение, чтобы после возврата в игру
    /// не всплыл один гигантский «удар» от мировых событий вроде отдыха у
    /// костра.
    pub fn tick(&mut self, now: Instant, count_damage: bool) {
        self.run(now, Force::None, count_damage);
    }

    /// Откатить всё немедленно, вернув покупки, за которые эффекты шли.
    ///
    /// Нужно там же, где `actions::release_all`: после аварийного выключателя
    /// тикать станет некому, и замедленная игра осталась бы замедленной до
    /// конца сессии. И на смерти игрока - прямой запрос 2026-08-24.
    ///
    /// Возвращённые погашения снимают свои карточки: отсчёт до конца эффекта
    /// иначе досчитывал бы до нуля того, чего уже нет.
    pub fn revert_all(&mut self) -> Vec<String> {
        self.revert(Force::All)
    }

    /// То же, но по смерти игрока: рунический обмен её переживает, см.
    /// `survives_death`.
    pub fn revert_on_death(&mut self) -> Vec<String> {
        self.revert(Force::ExceptSurvivors)
    }

    fn revert(&mut self, force: Force) -> Vec<String> {
        let doomed = |a: &Active| force == Force::All || !survives_death(a.key);
        let ids: Vec<String> = self
            .active
            .iter()
            .filter(|a| doomed(a))
            .map(|a| a.redemption_id.clone())
            .filter(|id| !id.is_empty())
            .collect();
        // `count_damage` тут ни на что не влияет: `run` зовёт `watch.poll`
        // только при `Force::None` (см. её же комментарий), а откат идёт
        // принудительно.
        self.run(Instant::now(), force, true);
        ids
    }

    fn run(&mut self, now: Instant, force: Force, count_damage: bool) {
        if self.active.is_empty() {
            return;
        }
        // Записку нельзя ставить прямо из `retain_mut` - `self` там уже занят.
        let mut notes: Vec<String> = Vec::new();
        self.active.retain_mut(|a| {
            let expired = match force {
                Force::None => a.until <= now,
                Force::All => true,
                Force::ExceptSurvivors => !survives_death(a.key),
            };
            // Пощажённого на принудительном откате не трогаем вовсе: тикнет
            // его обычный `tick` этого же кадра, и второй опрос урона в одном
            // кадре ему ни к чему.
            if force != Force::None && !expired {
                return true;
            }
            // `saturating_duration_since`, а не `-`: вычитание `Instant`
            // паникует на отрицательной разнице, а `panic = "abort"` в релизе
            // превращает панику в краш игры. Разница тут отрицательной быть не
            // должна, но «не должна» - не то, на что стоит ставить стрим.
            let phase = now.saturating_duration_since(a.started).as_secs_f32()
                * std::f32::consts::TAU
                / SWAY_PERIOD_SECS;
            match &mut a.restore {
                Restore::Speed => {
                    if let Ok(s) = speed() {
                        *s = if expired {
                            1.0
                        } else if a.key == "slow" {
                            SLOW_SPEED
                        } else {
                            FAST_SPEED
                        };
                    }
                }
                Restore::Stamina(saved) => {
                    if let Ok(p) = player() {
                        let data = &mut p.chr_ins.modules.data;
                        data.max_stamina = if expired {
                            *saved
                        } else if a.key == "stamina_half" {
                            (*saved / 2).max(1)
                        } else {
                            1
                        };
                        // Иначе полоска остаётся длиннее своего же максимума.
                        data.stamina = data.stamina.min(data.max_stamina);
                    }
                }
                Restore::Fov(base) => {
                    let factor =
                        if expired { None } else { Some(1.0 + SWAY_AMPLITUDE * phase.sin()) };
                    cam_write(base, factor);
                }
                Restore::Runes { watch, pending, net, gross, flushed } => {
                    let at = now;
                    if !expired {
                        // Слежение продолжается всегда - иначе после возврата
                        // из меню первый же кадр посчитал бы разницу с давно
                        // устаревшим снимком как один гигантский удар. Не
                        // считаем только `count_damage`.
                        let (dealt, taken) = watch.poll(at, count_damage);
                        *pending += dealt * RUNE_GAIN - taken * RUNE_LOSS;
                        gross.0 += dealt;
                        gross.1 += taken;
                    }
                    // Отдаём порциями, а не каждый кадр: счётчик рун иначе
                    // мельтешит, а по нему стример за эффектом и следит.
                    // На истечении отдаём остаток, чем бы он ни был.
                    if expired || at.duration_since(*flushed) >= RUNE_BATCH {
                        *flushed = at;
                        // Целую часть отдаём, дробную копим дальше - иначе
                        // слабые тики яда округлялись бы в ноль каждый.
                        let whole = pending.trunc();
                        if whole != 0.0 {
                            if let Ok(pgd) = player_game_data() {
                                // Скобки не для компилятора (унарный минус и
                                // так связывает сильнее `as`), а для читателя:
                                // это денежный путь, и «минус целого» тут
                                // должен читаться с одного взгляда.
                                pgd.rune_count = if whole > 0.0 {
                                    pgd.rune_count.saturating_add(whole as u32)
                                } else {
                                    pgd.rune_count.saturating_sub((-whole) as u32)
                                };
                                *net += whole as i64;
                                *pending -= whole;
                            }
                        }
                    }
                    if expired {
                        notes.push(format!(
                            "{}: {}{} ({} {:.0}, {} {:.0})",
                            crate::i18n::t("rune exchange"),
                            if *net >= 0 { "+" } else { "" },
                            net,
                            crate::i18n::t("dealt"),
                            gross.0,
                            crate::i18n::t("taken"),
                            gross.1
                        ));
                    }
                }
                Restore::Flasks { qty, pots } => {
                    if let Ok(pgd) = player_game_data() {
                        for (id, _, q) in flasks_mut(pgd) {
                            // Фляга, которой при покупке не было, - не наша:
                            // ни забирать, ни восстанавливать нечего.
                            if let Some((_, was)) = qty.iter().find(|(sid, _)| *sid == id) {
                                *q = if expired { *was } else { 0 };
                            }
                        }
                        set_pot_counts(pgd, pots, !expired);
                    }
                }
            }
            !expired
        });
        if let Some(note) = notes.pop() {
            self.note = Some(note);
        }
    }

    /// Разовые: применились и забылись.
    fn apply_once(&mut self, key: &'static str) -> Result<(), Rejected> {
        match key {
            "heal" | "restore_fp" | "one_hp" => {
                let data = &mut player()?.chr_ins.modules.data;
                match key {
                    "heal" => data.hp = data.max_hp,
                    "restore_fp" => data.fp = data.max_fp,
                    _ => data.hp = 1,
                }
                Ok(())
            }
            "flask_gift" => {
                let pgd = player_game_data()?;
                // Потолок - сколько фляг у игрока и так есть. Раньше стоял
                // общий FLASK_CAP, и покупка наливала больше, чем даёт сама
                // игра (жалоба живьём 2026-08-24).
                let (hp_max, fp_max) = (u32::from(pgd.max_hp_flask), u32::from(pgd.max_fp_flask));
                for (id, _, qty) in flasks_mut(pgd) {
                    let cap = if FLASK_HP.contains(&id) { hp_max } else { fp_max };
                    *qty = (*qty + FLASK_GIFT).min(cap);
                }
                Ok(())
            }
            "boss_heal" => {
                let data = boss_data()?;
                data.hp = (data.hp + data.max_hp / 2).min(data.max_hp);
                Ok(())
            }
            "boss_hp" => {
                let data = boss_data()?;
                // Полоска полной при этом не станет: лечим на десятую часть
                // нового максимума. Так и задумано - видно, что босса раздуло.
                data.max_hp = data.max_hp.saturating_mul(4);
                data.max_uncapped_hp = data.max_uncapped_hp.max(data.max_hp);
                data.hp = (data.hp + data.max_hp / 10).min(data.max_hp);
                Ok(())
            }
            _ => Err(Rejected::Unknown),
        }
    }
}

/// Временные: снимаем то, что придётся вернуть, и применяем первый раз -
/// дальше каждый кадр это делает `run`.
fn start_timed(key: &'static str) -> Result<Restore, Rejected> {
    match key {
        "slow" | "fast" => {
            speed()?;
            Ok(Restore::Speed)
        }
        "stamina_half" | "stamina_none" => {
            Ok(Restore::Stamina(player()?.chr_ins.modules.data.max_stamina))
        }
        "camera_sway" => Ok(Restore::Fov(cam_snapshot()?)),
        "rune_trade" => {
            let now = Instant::now();
            let mut watch = DamageWatch::default();
            // Снимаем базу сразу: без неё первый же кадр посчитал бы разницу
            // от нуля, то есть подарил бы полный запас HP каждого врага.
            watch.poll(now, false);
            Ok(Restore::Runes { watch, pending: 0.0, net: 0, gross: (0.0, 0.0), flushed: now })
        }
        "flask_lock" => {
            let pgd = player_game_data()?;
            let mut qty = Vec::new();
            let mut groups = Vec::new();
            for (id, group, q) in flasks_mut(pgd) {
                qty.push((id, *q));
                if let Ok(g) = usize::try_from(group) {
                    if !groups.contains(&g) {
                        groups.push(g);
                    }
                }
            }
            let counts = &pgd.equipment.equip_inventory_data.pot_items_count;
            let pots = groups.iter().filter_map(|g| counts.get(*g).map(|n| (*g, *n))).collect();
            Ok(Restore::Flasks { qty, pots })
        }
        _ => Err(Rejected::Unknown),
    }
}

// ---------------------------------------------------------------------------
// Доступ к игре
// ---------------------------------------------------------------------------
//
// Каждый резолв - отдельная функция: разным эффектам нужны разные синглтоны, а
// звать лишние незачем.

fn player() -> Result<&'static mut PlayerIns, Rejected> {
    unsafe { WorldChrMan::instance_mut() }
        .ok()
        .and_then(|w| w.main_player.as_mut())
        .map(|p| &mut **p)
        .ok_or(Rejected::NoPlayer)
}

fn player_game_data() -> Result<&'static mut PlayerGameData, Rejected> {
    // `NonNull`, а не `OwnedPtr`: игра заполняет его вместе с самим игроком, и
    // без игрока в мире мы сюда не доходим вовсе.
    Ok(unsafe { player()?.player_game_data.as_mut() })
}

fn speed() -> Result<&'static mut f32, Rejected> {
    unsafe { CSFlipper::instance_mut() }.map(|f| &mut f.game_speed).map_err(|_| Rejected::NoPlayer)
}

/// Заряды всех фляг игрока: `(param_id, pot_group, &mut заряды)`.
///
/// Фляга - обычный предмет инвентаря, и число зарядов лежит у неё в
/// `quantity`. Ставить его в ноль и обратно можно свободно: своей функции
/// «отобрать фляги» у игры нет, а `MapItemMan` крейт наружу не отдаёт вовсе.
fn flasks_mut(pgd: &mut PlayerGameData) -> impl Iterator<Item = (u32, i32, &mut u32)> {
    pgd.equipment.equip_inventory_data.items_data.items_mut().filter_map(|item| {
        let id = item.item_id;
        (id.category() == ItemCategory::Goods
            && (FLASK_HP.contains(&id.param_id()) || FLASK_FP.contains(&id.param_id())))
        .then_some((id.param_id(), item.pot_group, &mut item.quantity))
    })
}

/// Обнулить счётчики групп фляг или вернуть их на место.
///
/// **Одной `quantity` не хватило** (жалоба живьём 2026-08-24): иконка фляги
/// пропадала, но одну всё равно давало выпить. Игра держит рядом ещё и
/// `pot_items_count` по группам предметов-«сосудов», и опустошать надо оба
/// счётчика. Не сработает и это - следующим подозреваемым идёт слот быстрого
/// доступа (`EquipItemData`), где количество может быть закэшировано отдельно.
fn set_pot_counts(pgd: &mut PlayerGameData, pots: &[(usize, u32)], zero: bool) {
    let counts = &mut pgd.equipment.equip_inventory_data.pot_items_count;
    for (group, was) in pots {
        if let Some(slot) = counts.get_mut(*group) {
            *slot = if zero { 0 } else { *was };
        }
    }
}

/// Данные того, у кого сейчас полоска босса.
///
/// Первый непустой: на двойном боссе выбор произвольный, но покупка и карточка
/// на экране всё равно одна.
fn boss_data() -> Result<&'static mut CSChrDataModule, Rejected> {
    let handle = boss_handle().ok_or(Rejected::NoBoss)?;
    let world = unsafe { WorldChrMan::instance_mut() }.map_err(|_| Rejected::NoPlayer)?;
    world.chr_ins_by_handle_mut(&handle).map(|chr| &mut *chr.modules.data).ok_or(Rejected::NoBoss)
}

fn boss_handle() -> Option<FieldInsHandle> {
    let fe = unsafe { CSFeManImp::instance() }.ok()?;
    fe.boss_health_displays.iter().map(|d| d.field_ins_handle).find(|h| !h.is_empty())
}

/// Снимок нужного поля `LockCamParam` по всем строкам.
///
/// Правятся ВСЕ строки разом: какая сейчас активна, решает игра (обычная
/// камера, лок-он, разговор, арена), и угадывать это незачем.
fn cam_snapshot() -> Result<Vec<(u32, f32)>, Rejected> {
    let repo = unsafe { SoloParamRepository::instance() }.map_err(|_| Rejected::NoParams)?;
    let base: Vec<(u32, f32)> =
        repo.rows::<LockCamParam>().map(|(id, row)| (id, row.cam_fov_y())).collect();
    // Парамы ещё не загружены - крутить нечего, и пустой снимок стал бы
    // эффектом, который молча ничего не делает.
    if base.is_empty() {
        return Err(Rejected::NoParams);
    }
    Ok(base)
}

/// `factor = None` - вернуть снимок как был.
///
/// `zip`, а не поиск по id: парам в рантайме не меняется, порядок строк тот
/// же самый, и линейный проход дешевле квадратичного на несколько сотен строк
/// каждый кадр. Id всё равно сверяем - если порядок вдруг разойдётся, лучше
/// пропустить строку, чем записать в неё чужое число.
fn cam_write(base: &[(u32, f32)], factor: Option<f32>) {
    let Ok(repo) = (unsafe { SoloParamRepository::instance_mut() }) else {
        return;
    };
    for ((id, row), (base_id, was)) in repo.rows_mut::<LockCamParam>().zip(base.iter()) {
        if id == *base_id {
            row.set_cam_fov_y(was * factor.unwrap_or(1.0));
        }
    }
}

/// Слежение за HP всех персонажей мира - порт `elden` вместе с его причинами.
///
/// **Урон измеряется разницей HP, а не детуром.** В `elden` он считается ровно
/// так же (`last_hp`, `track_hp`), а его детур (`OnAttack`) нужен для другого -
/// узнать, КТО ударил. Ставить такой же детур мы не будем, и не только из
/// принципа «мод не патчит игровой код»: хук на этой функции может уже
/// держать другой мод, а два трамплина на одном прологе - готовый конфликт
/// между модами, которые запускают вместе.
///
/// Владельца удара берём типизированным полем `ChrIns::last_hit_by` - это
/// третья, самая слабая ступень того же поиска в `elden`, но единственная, что
/// обходится без патча.
///
/// **У выстрела `last_hit_by` называет не стрелка, а САМ СНАРЯД**
/// (`FieldInsType::Bullet`). Первая версия требовала `last_hit_by == игрок`, и
/// живьём это дало «руны только списываются, никогда не начисляются» (жалоба
/// 2026-08-24): вся стрельба, а челлендж весь про неё, не засчитывалась ни разу.
///
/// Стрелок у снаряда есть, и тоже типизированный -
/// `CSBulletTargetingSystemOwner::owner_chr_handle`. Одна тонкость: к моменту,
/// когда мы видим упавшие HP, стрелы уже нет, игра уничтожает её при попадании.
/// Поэтому владельцев запоминаем, ПОКА СНАРЯД ЛЕТИТ (`bullets`), и держим ещё
/// `BULLET_MEMORY`. Это и есть ответ на «в elden урон из лука считается»: там
/// стрелка называет детур, здесь - сам снаряд, пока он в воздухе.
///
/// Итоговое правило: **удар наш, пока не доказано обратное.** Доказательством
/// считается только названный ДРУГОЙ персонаж - прямо в `last_hit_by` или как
/// владелец снаряда. Всё, чего мы не знаем (снаряд уже забыт, яд, огонь,
/// падение), идёт игроку.
///
/// Что взято у `elden` дословно, потому что каждая часть закрывает найденный
/// им живьём баг:
///
/// - **обход ВСЕХ наборов**, а не только тех, кого игра показывает: иначе
///   добитый за кадром враг и урон по площади не считаются вовсе;
/// - **`HashMap` с кадровой меткой и подметанием в конце**: без него карта
///   росла бы всю сессию, а выгрузка-загрузка врага читалась бы как один
///   гигантский удар;
/// - **забываем умершего** (`hp <= 0` при живом прошлом), иначе переселение
///   слота на нового врага читается уроном;
/// - **`RESYNC_GAP`**: пауза между вызовами значит, что была загрузка, и все
///   прошлые HP протухли. Одного его мало: мод тикает на `Present`, а тот идёт
///   и на загрузочном экране, так что паузы может не быть вовсе. Поэтому
///   слежка забывает всё ещё и тогда, когда игрока в мире нет - `elden` для
///   того же держит отдельный `reset()` на смене карты.
#[derive(Default)]
pub struct DamageWatch {
    last_hp: HashMap<FieldInsHandle, (i32, u64)>,
    /// Чей снаряд: хэндл снаряда -> хэндл стрелка и когда видели последний раз.
    bullets: HashMap<FieldInsHandle, (FieldInsHandle, Instant)>,
    player_hp: Option<i32>,
    frame: u64,
    last_poll: Option<Instant>,
}

impl DamageWatch {
    /// Сколько урона нанесено игроком и получено им с прошлого вызова.
    ///
    /// В обе величины сама собой попадает **всё, что меняет HP** - удар, яд,
    /// гниль, горение, чёрное пламя, падение. Детур на функцию удара их не
    /// видит вовсе, так что разница по HP тут не компромисс, а более полный
    /// ответ.
    ///
    /// `count = false` - обновить весь учёт (базовые HP, снаряды, чистку),
    /// но вернуть ноль. **Найдено живьём 2026-08-24: отдых у костра начисляет
    /// десятки тысяч рун за раз** - рестарт области при отдыхе массово убирает
    /// врагов, у которых `last_hit_by` мог остаться от прошлого боя, и правило
    /// «удар наш, пока не доказано обратное» списывает это на игрока. `menu_open`
    /// накрывает и это, и любой другой мировой сброс за меню (отдых - тоже
    /// меню). Не звать `forget()`: `last_hp` должен остаться свежим, иначе
    /// закрытие меню тут же дало бы такой же ложный залп по разнице с
    /// протухшим снимком.
    fn poll(&mut self, now: Instant, count: bool) -> (f32, f32) {
        // Была пауза - между вызовами могла пройти загрузка, и прошлые HP
        // ничего не значат. Пропускаем кадр целиком.
        let stale = self.last_poll.is_none_or(|t| now.duration_since(t) >= RESYNC_GAP);
        self.last_poll = Some(now);
        if stale {
            self.last_hp.clear();
            self.bullets.clear();
            self.player_hp = None;
        }
        self.frame = self.frame.wrapping_add(1);
        let frame = self.frame;

        // Игрока в мире нет - идёт загрузка или главное меню. Всё, что мы
        // помним, протухло: после загрузки тот же слот хэндла достанется
        // другому существу, и его HP прочитались бы как урон. `RESYNC_GAP` тут
        // не спасает - `Present` идёт и на загрузочном экране, паузы между
        // вызовами не возникает.
        let mut forget = || {
            self.last_hp.clear();
            self.bullets.clear();
            self.player_hp = None;
            (0.0, 0.0)
        };
        let Ok(world) = (unsafe { WorldChrMan::instance() }) else {
            return forget();
        };
        let Some(player) = world.main_player.as_ref() else {
            return forget();
        };
        let me = player.chr_ins.field_ins_handle;

        // Игрок. Рост HP - это лечение, его не считаем.
        let hp = player.chr_ins.modules.data.hp;
        let taken = match self.player_hp.replace(hp) {
            Some(prev) if prev > 0 && hp < prev => (prev - hp) as f32,
            _ => 0.0,
        };

        // Пока снаряды в воздухе - запоминаем, чьи они. После попадания игра их
        // уничтожает, а `last_hit_by` у цели продолжает на них указывать.
        if let Ok(bullets) = unsafe { CSBulletManager::instance() } {
            for bullet in bullets.bullets() {
                let owner = bullet.targeting_owner.owner_chr_handle;
                if !owner.is_empty() {
                    self.bullets.insert(bullet.field_ins_handle, (owner, now));
                }
            }
        }
        self.bullets.retain(|_, (_, seen)| now.duration_since(*seen) < BULLET_MEMORY);

        let mut dealt = 0.0;
        let open = world.open_field_chr_set.base.characters();
        let rest = world.chr_sets.iter().flatten().flat_map(|s| s.characters());
        for chr in open.chain(rest) {
            let handle = chr.field_ins_handle;
            if handle == me {
                continue;
            }
            let hp = chr.modules.data.hp;
            let prev = self.last_hp.insert(handle, (hp, frame)).map(|(v, _)| v);
            let Some(prev) = prev else { continue };
            // Умер - забываем сразу, до того как слот достанется другому.
            // Сам добивающий удар при этом считается: `prev` уже прочитан.
            if hp <= 0 && prev > 0 {
                self.last_hp.remove(&handle);
            }
            // Чужая драка рунами не оплачивается, всё прочее - наше (см. шапку).
            let by = chr.last_hit_by;
            let attacker = match by.selector.field_ins_type() {
                Some(FieldInsType::Chr) => Some(by),
                Some(FieldInsType::Bullet) => self.bullets.get(&by).map(|(owner, _)| *owner),
                _ => None,
            };
            let someone_else = attacker.is_some_and(|who| who != me);
            if hp < prev && !someone_else {
                dealt += (prev - hp) as f32;
            }
        }
        // Подметаем всё, чего в этом кадре не встретили: иначе карта растёт
        // всю сессию, а вернувшийся из выгрузки враг даёт мнимый удар.
        self.last_hp.retain(|_, (_, seen)| *seen == frame);
        if count { (dealt, taken) } else { (0.0, 0.0) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ключи уникальны и подписаны на обоих языках: по ключу ищется строка в
    /// файле наград, а дубль означал бы, что одна из наград недостижима.
    #[test]
    fn table_is_well_formed() {
        for e in EFFECT_TABLE {
            assert!(!e.key.is_empty(), "пустой ключ");
            assert!(!e.label.is_empty(), "{} без подписи", e.key);
            assert!(e.default_secs <= MAX_SECS, "{} длиннее потолка", e.key);
            assert_eq!(
                EFFECT_TABLE.iter().filter(|o| o.key == e.key).count(),
                1,
                "ключ {} встречается дважды",
                e.key
            );
        }
    }

    /// У каждой строки таблицы обязана быть ветка в `apply_once` или
    /// `start_timed`. Ловит забытую ветку при добавлении эффекта: живьём это
    /// выглядело бы как награда, которая молча ничего не делает.
    ///
    /// Игры тут нет, поэтому проверяется только то, что ключ распознан:
    /// `Unknown` - забытая ветка, всё остальное - отказ по данным игры.
    #[test]
    fn every_effect_has_a_branch() {
        let mut s = EffectState::default();
        for e in EFFECT_TABLE {
            let err = if e.default_secs > 0 {
                start_timed(e.key).err()
            } else {
                s.apply_once(e.key).err()
            };
            assert!(
                !matches!(err, Some(Rejected::Unknown)),
                "у эффекта {} нет ветки исполнения",
                e.key
            );
        }
    }

    /// Две ступени одного механизма не должны копиться: вторая запись сняла бы
    /// снимок с уже испорченного значения, и откат вернул бы не то.
    ///
    /// Игровой памяти в тесте нет, поэтому проверяется сама таблица групп -
    /// именно она и решает.
    #[test]
    fn two_effects_of_one_group_cannot_stack() {
        assert_eq!(group("stamina_half"), group("stamina_none"));
        assert_eq!(group("slow"), group("fast"));
        assert_ne!(group("slow"), group("stamina_half"));
        // У разовых группы нет - им нечего откатывать, и мешать друг другу
        // они не могут.
        for e in EFFECT_TABLE {
            assert_eq!(
                group(e.key).is_empty(),
                e.default_secs == 0,
                "у эффекта {} группа не совпадает с его видом",
                e.key
            );
        }
    }

    #[test]
    fn unknown_key_is_rejected() {
        let mut s = EffectState::default();
        assert!(matches!(s.apply("нет-такого", 0, ""), Err(Rejected::Unknown)));
        assert!(key_of("нет-такого").is_none());
        assert!(key_of("slow").is_some());
    }

    /// Разовый эффект срока не имеет, у временного он есть - на этом стоит и
    /// разбор в `rewards.rs`, и ползунок в настройках.
    #[test]
    fn timed_and_instant_are_told_apart() {
        assert_eq!(default_secs("heal"), 0);
        assert!(default_secs("slow") > 0);
        assert_eq!(default_secs("нет-такого"), 0);
    }

    /// Смерть снимает эффекты, но рунический обмен её переживает - он не
    /// мешает играть, а считает итог боя (уточнение 2026-08-24).
    ///
    /// Правило проверяется по таблице целиком: щадить можно только временный
    /// эффект (разовому нечего переживать), и щадящийся обязан быть ровно один
    /// - список исключений, который растёт молча, перестаёт быть исключением.
    #[test]
    fn only_the_rune_exchange_survives_death() {
        assert!(survives_death("rune_trade"));
        let spared: Vec<&str> =
            EFFECT_TABLE.iter().map(|e| e.key).filter(|k| survives_death(k)).collect();
        assert_eq!(spared, vec!["rune_trade"], "щадящихся стало больше одного: {spared:?}");
        for e in EFFECT_TABLE {
            if survives_death(e.key) {
                assert!(e.default_secs > 0, "{} разовый - переживать ему нечего", e.key);
            }
        }
    }

    /// Диапазоны фляг не должны пересекаться: одна строка попала бы в снимок
    /// дважды, и откат вернул бы её дважды.
    #[test]
    fn flask_ranges_do_not_overlap() {
        assert!(FLASK_HP.end() < FLASK_FP.start());
    }

    /// Рунический обмен: нанёс - начислили, получил - списали, и списывают
    /// дороже, чем начисляют. Числа заданы прямым запросом, и тест держит
    /// именно их - перепутать местами множители на глаз нельзя ничем.
    #[test]
    fn rune_exchange_pays_less_than_it_charges() {
        let net = |dealt: f32, taken: f32| dealt * RUNE_GAIN - taken * RUNE_LOSS;
        assert_eq!(net(1000.0, 0.0), 600.0);
        assert_eq!(net(0.0, 1000.0), -800.0);
        // Размен «удар на удар» обязан быть в минус: иначе награда стала бы
        // бесконечным станком для рун.
        assert!(net(500.0, 500.0) < 0.0);
        assert!(net(1.0, 1.0) < 0.0, "списание обязано быть дороже начисления");
    }

    /// Волна качки за период обязана сходить в обе стороны и вернуться в
    /// единицу: множитель, застрявший на одном значении, - это не качка, а
    /// просто другой обзор.
    #[test]
    fn sway_swings_both_ways_and_returns_to_one() {
        let factor = |secs: f32| {
            let phase = secs * std::f32::consts::TAU / SWAY_PERIOD_SECS;
            1.0 + SWAY_AMPLITUDE * phase.sin()
        };
        assert!((factor(0.0) - 1.0).abs() < 1e-5, "в нуле обзор родной");
        assert!((factor(SWAY_PERIOD_SECS) - 1.0).abs() < 1e-4, "за период возвращается");
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for ms in 0..(SWAY_PERIOD_SECS * 1000.0) as u32 {
            let f = factor(ms as f32 / 1000.0);
            lo = lo.min(f);
            hi = hi.max(f);
        }
        assert!(lo < 1.0 - SWAY_AMPLITUDE + 0.01, "вниз не доходит: {lo}");
        assert!(hi > 1.0 + SWAY_AMPLITUDE - 0.01, "вверх не доходит: {hi}");
    }
}
