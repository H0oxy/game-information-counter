//! Спавн врага за баллы канала - куратор-список, позиция вокруг игрока, TTL.
//!
//! Самая рискованная правка в моде: первая мутация игровой памяти
//! (`WorldChrMan::instance_mut`), а не только чтение. Спавн просит игру
//! создать `ChrIns` асинхронно - `spawn_debug_character` лишь ставит флаг,
//! само существо появляется у движка на следующем такте
//! (`debug_chr_creator.last_created_chr`).
//!
//! Файл разбит на две половины: чистая математика/куратор-список сверху,
//! протестирована; `SpawnState` снизу требует настоящего `&mut WorldChrMan` и
//! тестами не покрыта - тот же принцип, что у `enemies.rs::native_tags`.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use eldenring::cs::{
    BuddyParam, BuddyStoneParam, CSCamera, CSHavokMan, CSPhysWorld, ChrDebugSpawnRequest, ChrIns,
    EnemyIns, FieldInsHandle, NpcParam, NpcThinkParam, PlayerIns, SoloParamRepository, SpEffectParam,
    SummonBuddyGroup, WorldChrMan,
};
use fromsoftware_shared::Superclass;
use eldenring::cs::ChrInsExt;
use eldenring::param::SP_EFFECT_PARAM_ST;
use eldenring::position::{HavokPosition, PositionDelta};
use fromsoftware_shared::FromStatic;

/// Одна строка куратор-списка: что можно заспавнить.
///
/// Хранится только `npc_param_id` - остальное выводится из него, см.
/// `chr_id()` и `think_param_id()`. Три отдельных поля были ошибкой: их
/// пришлось бы держать согласованными руками, а рассогласование - это краш.
pub struct SpawnEntry {
    pub key: &'static str,
    /// Английское название, оно же ключ перевода (см. `i18n`).
    pub label: &'static str,
    pub npc_param_id: i32,
    /// Насколько тяжело с ним драться. Босс - отдельная ступень, а не флаг
    /// поверх сложности: у него своя полоска здоровья, своя цена в баллах и
    /// свой фильтр в окне наград.
    ///
    /// Разбиение на лёгких/средних/сложных нужно награде «случайный враг»:
    /// «случайный кто угодно» на канале с дорогими наградами - это лотерея,
    /// в которой зритель платит одинаково за импа и за рыцаря Горнила.
    pub tier: Tier,
    /// Призывается СВОИМ пеплом: здесь его триггер, а `npc_param_id` не
    /// используется. Так призывается Ложная Слеза - двойник на модели игрока,
    /// которого обычным путём не собрать: строки её блока трогать нельзя, от
    /// правки игра падает.
    ///
    /// `0` у всех остальных - обычный путь через занятый блок.
    pub ash: i32,
    /// Существо на МОДЕЛИ ИГРОКА: здесь строка его внешности и снаряжения, а
    /// `think` задан явно - вывести его из номера нельзя.
    ///
    /// Отладочный создатель такое не собирает: он выставляет имя ассета, три
    /// номера и позицию, а тело двойника складывается из частей снаряжения.
    /// Четыре августовских захода упёрлись именно в это. Система призыва
    /// собирает его сама - ей передаётся и внешность
    /// (`npc_player_init_param_id`), и анимация материализации, которых у
    /// отладочного пути не было вовсе.
    ///
    /// `0` - обычное существо со своей моделью.
    pub init: i32,
    /// Строка поведения, заданная явно. Нужна только двойникам: у них номер
    /// строки парама из чужого диапазона, и усечением её не найти.
    pub think: i32,
}

/// Ступень сложности. Порядок вариантов - по нарастанию, на него опирается и
/// подпись в списке, и сортировка списка в окне награды (`Ord`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Tier {
    Easy,
    Normal,
    Hard,
    Boss,
}

impl Tier {
    /// Ступень римской цифрой: I - лёгкий, IV - босс. Слово рядом с каждым из
    /// двух сотен имён занимало больше места, чем значило, а в узком столбце
    /// списка места нет вовсе (запрос 2026-08-23). Перевода не требует.
    pub fn label(self) -> &'static str {
        match self {
            Tier::Easy => "I",
            Tier::Normal => "II",
            Tier::Hard => "III",
            Tier::Boss => "IV",
        }
    }
}

/// Ступень по строке поиска: «I», «II», «III», «IV». Совпадение точное, а не
/// вхождение - «I» иначе нашла бы заодно III и IV. Перебивает поиск по имени:
/// на латинице «i» встречается в половине имён, и без этого ступень было бы
/// не отфильтровать.
pub fn tier_of_needle(needle: &str) -> Option<Tier> {
    let n = needle.trim();
    [Tier::Easy, Tier::Normal, Tier::Hard, Tier::Boss]
        .into_iter()
        .find(|t| t.label().eq_ignore_ascii_case(n))
}

/// Строка списка. Четыре аргумента вместо четырёх именованных полей: строк
/// под две сотни, и полное имя каждого поля в каждой из них - это стена, а не
/// таблица.
const fn e(key: &'static str, label: &'static str, npc_param_id: i32, tier: Tier) -> SpawnEntry {
    SpawnEntry { key, label, npc_param_id, tier, ash: 0, init: 0, think: 0 }
}

/// Существо на модели игрока: номера снимаются с его ванильного размещения и
/// задаются целиком, потому что вывести их не из чего.
const fn pm(
    key: &'static str,
    label: &'static str,
    npc_param_id: i32,
    think: i32,
    init: i32,
    tier: Tier,
) -> SpawnEntry {
    SpawnEntry { key, label, npc_param_id, tier, ash: 0, init, think }
}

/// Строка списка для того, у кого есть свой пепел. Собрать его как обычного
/// врага нельзя, а призвать - можно.
const fn ash(key: &'static str, label: &'static str, trigger: i32, tier: Tier) -> SpawnEntry {
    SpawnEntry { key, label, npc_param_id: 0, tier, ash: trigger, init: 0, think: 0 }
}

impl SpawnEntry {
    /// Из какого ассета лепить модель. Движок собирает из этого числа имя
    /// `c{:0>4}` (см. `spawn_debug_character`), а первые четыре цифры
    /// `npc_param_id` - это ровно оно: `35001010` -> `c3500` (скелет).
    ///
    /// Отдельной колонкой это было бы ещё одним полем, которое можно
    /// рассогласовать; вычисленное - рассогласовать нельзя.
    pub fn chr_id(&self) -> i32 {
        self.npc_param_id / 10_000
    }

    pub fn boss(&self) -> bool {
        self.tier == Tier::Boss
    }

    /// Подходит ли под строку поиска и фильтр боссов. Ищем и по переводу, и
    /// по английскому названию: список подписан на языке интерфейса, но
    /// набирать человек может и знакомое имя из вики.
    pub fn matches(&self, needle: &str, bosses_only: bool) -> bool {
        if bosses_only && !self.boss() {
            return false;
        }
        if let Some(tier) = tier_of_needle(needle) {
            return self.tier == tier;
        }
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return true;
        }
        crate::i18n::t(self.label).to_lowercase().contains(&needle)
            || self.label.to_lowercase().contains(&needle)
            || self.key.contains(&needle)
    }
}

/// Куратор-список: во что баллы канала разрешено превращать. Не сырой ввод
/// ID - зритель мог бы вписать что угодно, включая комбинацию, валящую
/// движок. Добавить врага - дописать строку сюда, больше нигде ничего менять
/// не надо: и persistence (`twitch::rewards`), и UI (`settings::tab_rewards`)
/// читают эту таблицу.
///
/// **Числа сняты с рабочих источников, не выдуманы.** Часть - дамп живой игры
/// (Cheat Engine, таблица World Characters, 2026-08-20), часть - таблица
/// спавна streamtoearn.io/elden-ring, где те же пары `chr_id + npc_param_id`
/// используются работающим модом. Выдуманный id либо не спавнит ничего, либо
/// валит игру, поэтому сюда попадает только подтверждённое.
///
/// **Граница проходит по «нападает или нет», а не по зоологии.** Волки, псы,
/// крысы, крабы, медведь и кабан в списке есть - они атакуют. Убегающих и
/// декоративных нет: олени, черепахи, скакуны-кролики, козы, совы, орлы,
/// голуби, стрекозы, скарабеи. Нет и того, что не существо: осадные машины,
/// ездовые лошади, трупы боссов, Ходячий мавзолей. Дружелюбных NPC
/// (торговцы, Мелина, Ранни, Юра, союзные фантомы) в списке нет и не было.
///
/// Порядок - по нарастанию угрозы: обычные, мини-боссы, легендарные.
pub const SPAWN_TABLE: &[SpawnEntry] = &[
    // --- Рядовые противники ---
    e("imp", "Imp", 30800012, Tier::Easy),
    e("flower", "Miranda Blossom", 44800010, Tier::Easy),
    e("living_jar", "Living Jar", 44910033, Tier::Easy),
    e("living_jar_warrior", "Living Jar Warrior", 44900011, Tier::Normal),
    e("hand", "Creeping Hand", 42500021, Tier::Easy),
    e("living_mass", "Putrid Flesh", 41700110, Tier::Easy),
    e("noble", "Wandering Noble", 43001010, Tier::Easy),
    e("noble_axe", "Noble with Axe", 43005010, Tier::Easy),
    e("noble_veteran", "Veteran Noble", 43003010, Tier::Normal),
    e("putrid_corpse", "Putrid Corpse", 36610012, Tier::Easy),
    e("skeleton", "Skeleton", 35001010, Tier::Easy),
    e("skeleton_tough", "Tough Skeleton", 35003010, Tier::Normal),
    e("skeleton_elite", "Elite Skeleton", 35004032, Tier::Normal),
    e("catacomb_skeleton", "Catacomb Skeleton", 35100006, Tier::Easy),
    e("giant_skeleton", "Giant Skeleton", 30600020, Tier::Hard),
    e("skeleton_torso", "Giant Skeleton Torso", 49600032, Tier::Normal),
    e("godrick_soldier", "Godrick Soldier", 43110010, Tier::Easy),
    e("godrick_foot", "Godrick Foot Soldier", 43710010, Tier::Easy),
    e("leyndell_soldier", "Leyndell Soldier", 43130030, Tier::Normal),
    e("radahn_soldier", "Radahn Soldier", 43140040, Tier::Normal),
    e("godrick_knight", "Godrick Knight", 43510010, Tier::Normal),
    e("cuckoo_knight", "Cuckoo Knight", 43520020, Tier::Normal),
    e("leyndell_knight", "Leyndell Knight", 43530020, Tier::Hard),
    e("redmane_knight", "Redmane Knight", 43540040, Tier::Hard),
    e("haligtree_knight", "Haligtree Knight", 43560034, Tier::Hard),
    e("banished_knight", "Banished Knight", 30100014, Tier::Hard),
    e("exile", "Exile Soldier", 30000006, Tier::Easy),
    e("exile_large", "Large Exile Soldier", 30200014, Tier::Normal),
    e("kaiden", "Kaiden Sellsword", 40500010, Tier::Normal),
    e("highwayman", "Highwayman", 43770012, Tier::Easy),
    e("vulgar_militia", "Vulgar Militia", 43210012, Tier::Easy),
    e("marionette", "Marionette Soldier", 38500012, Tier::Normal),
    e("avionette", "Avionette Soldier", 38600020, Tier::Normal),
    e("fire_monk", "Fire Monk", 39000020, Tier::Normal),
    e("fire_prelate", "Fire Prelate", 39100032, Tier::Hard),
    e("man_serpent", "Man-Serpent", 39500038, Tier::Normal),
    e("azula_beastman", "Azula Beastman", 39700072, Tier::Hard),
    e("cleanrot_knight", "Cleanrot Knight", 38000031, Tier::Hard),
    e("kindred_of_rot", "Kindred of Rot", 38100040, Tier::Hard),
    e("depraved_perfumer", "Depraved Perfumer", 37000010, Tier::Normal),
    e("perfumer", "Perfumer", 37010030, Tier::Normal),
    e("glintstone_sorcerer", "Glintstone Sorcerer", 37020020, Tier::Easy),
    e("battlemage", "Battlemage", 37040020, Tier::Normal),
    e("demihuman", "Demi-Human", 41000010, Tier::Easy),
    e("demihuman_chief", "Demi-Human Chief", 41200009, Tier::Normal),
    e("misbegotten", "Misbegotten", 34500012, Tier::Easy),
    e("albinauric", "Albinauric", 34700020, Tier::Easy),
    e("guardian", "Guardian", 36500012, Tier::Normal),
    e("oracle_envoy", "Oracle Envoy", 36100034, Tier::Easy),
    e("nox_swordstress", "Nox Swordstress", 33000062, Tier::Normal),
    e("silver_tear", "Silver Tear", 33200062, Tier::Normal),
    e("ancestral_follower", "Ancestral Follower", 33600020, Tier::Normal),
    e("grave_warden", "Grave Warden Duelist", 34000030, Tier::Normal),
    e("sanguine_noble", "Sanguine Noble", 35500020, Tier::Hard),
    // Враждебные NPC со СВОЕЙ моделью. Юра, Патчес, Бернал, Роджер и прочие
    // такие же сюда не годятся вовсе: они слеплены из модели ИГРОКА
    // (`c0000` + `CharaInitParam`), а спавн на ней не работает - проверено
    // живьём четырьмя заходами 2026-08-23, тело создаётся без модели.
    // Id по конвенции `chr_id * 10000`; подтверждать их - кнопкой
    // «Проверить список» у запущенной игры, а не дампом.
    e("blaidd", "Blaidd", 20100000, Tier::Hard),
    e("elmer", "Elmer of the Briar", 31000000, Tier::Hard),
    e("fallen_hawks", "Fallen Hawks Soldier", 70000065, Tier::Normal),
    e("royal_revenant", "Royal Revenant", 40200003, Tier::Hard),
    e("abductor_virgin", "Abductor Virgin", 44700020, Tier::Hard),
    e("basilisk", "Basilisk", 41500030, Tier::Easy),
    e("watchdog", "Erdtree Burial Watchdog", 42600100, Tier::Normal),
    e("elder_lion", "Elder Lion", 42700014, Tier::Normal),
    e("warhawk", "Warhawk", 42100014, Tier::Normal),
    e("land_octopus", "Land Octopus", 42300010, Tier::Easy),
    e("giant_ant", "Giant Ant", 42800054, Tier::Easy),
    e("pumpkin_head", "Mad Pumpkin Head", 43400010, Tier::Normal),
    e("wormface", "Wormface", 45700030, Tier::Normal),
    e("omen", "Omen", 21400012, Tier::Hard),
    // --- Рядовые, добавленные 2026-08-23 ---
    e("wolf", "Wolf", 40700010, Tier::Easy),
    e("rat", "Rat", 40800010, Tier::Easy),
    e("giant_rat", "Giant Rat", 40900010, Tier::Easy),
    e("slug", "Slug", 40400030, Tier::Easy),
    e("snail", "Snail", 41400038, Tier::Easy),
    e("rot_larva", "Kindred of Rot Larva", 20410040, Tier::Easy),
    e("land_squirt", "Land Squirt", 44400010, Tier::Easy),
    e("giant_land_squirt", "Giant Land Squirt", 44410020, Tier::Easy),
    e("miranda_sprout", "Miranda Sprout", 44810020, Tier::Easy),
    e("rotten_miranda", "Rotten Miranda Blossom", 44820040, Tier::Easy),
    e("giant_crab", "Giant Crab", 22700012, Tier::Easy),
    e("stray", "Stray", 41610010, Tier::Easy),
    e("stonedigger", "Stonedigger", 43820010, Tier::Easy),
    e("glintstone_digger", "Glintstone Digger", 43830020, Tier::Easy),
    e("guilty", "Guilty", 43810020, Tier::Easy),
    e("raya_lucaria_foot", "Raya Lucaria Foot Soldier", 43720020, Tier::Easy),
    e("demihuman_shaman", "Demi-Human Shaman", 41100020, Tier::Easy),
    e("man_bat", "Man-Bat", 42000020, Tier::Normal),
    e("operatic_bat", "Operatic Bat", 42010020, Tier::Normal),
    e("fingercreeper", "Fingercreeper", 42400020, Tier::Normal),
    e("giant_fingercreeper", "Giant Fingercreeper", 42410100, Tier::Hard),
    e("disciple_of_rot", "Disciple of Rot", 43850040, Tier::Normal),
    e("scaly_misbegotten", "Scaly Misbegotten", 34510030, Tier::Normal),
    e("oracle_envoy_large", "Large Oracle Envoy", 36200034, Tier::Normal),
    e("oracle_envoy_giant", "Giant Oracle Envoy", 36300054, Tier::Hard),
    e("putrid_corpse_large", "Large Putrid Corpse", 36620040, Tier::Normal),
    e("demihuman_large", "Large Demi-Human", 41010020, Tier::Normal),
    e("stray_large", "Large Stray", 41600024, Tier::Normal),
    e("azula_stray", "Azula Stray", 41620072, Tier::Normal),
    e("bloodbane_stray", "Bloodbane Stray", 41640030, Tier::Normal),
    e("rotten_stray", "Rotten Stray", 41660040, Tier::Normal),
    e("raya_lucaria_soldier", "Raya Lucaria Soldier", 43120020, Tier::Normal),
    e("mausoleum_soldier", "Mausoleum Soldier", 43150052, Tier::Normal),
    e("haligtree_soldier", "Haligtree Soldier", 43160056, Tier::Hard),
    e("mausoleum_knight", "Mausoleum Knight", 43550052, Tier::Hard),
    e("leyndell_foot", "Leyndell Foot Soldier", 43730030, Tier::Normal),
    e("radahn_foot", "Radahn Foot Soldier", 43740040, Tier::Normal),
    e("haligtree_foot", "Haligtree Foot Soldier", 43760056, Tier::Hard),
    e("starcaller", "Starcaller", 43800032, Tier::Normal),
    e("giant_black_crab", "Giant Black Crab", 22720020, Tier::Normal),
    e("giant_death_crab", "Giant Death Crab", 22760020, Tier::Hard),
    e("giant_beast_skeleton", "Giant Beast Skeleton", 30610072, Tier::Hard),
    e("celebrant", "Dominula Celebrant", 30700030, Tier::Normal),
    e("albinauric_archer", "Albinauric Archer", 31700052, Tier::Normal),
    e("giant_silver_tear", "Giant Silver Tear", 33300040, Tier::Hard),
    e("putrid_ancestral", "Putrid Ancestral Follower", 33610062, Tier::Normal),
    e("ancestral_shaman", "Ancestral Follower Shaman", 33700052, Tier::Normal),
    e("albinauric_large", "Large Albinauric", 34715100, Tier::Normal),
    e("elder_albinauric", "Elder Albinauric", 36700042, Tier::Normal),
    e("graven_school", "Graven School", 37300040, Tier::Normal),
    e("clayman", "Clayman", 37500060, Tier::Normal),
    e("blackflame_monk", "Blackflame Monk", 39010038, Tier::Hard),
    e("revenant_follower", "Revenant Follower", 40000030, Tier::Normal),
    e("giant_putrid_flesh", "Giant Putrid Flesh", 41710430, Tier::Normal),
    e("land_octopus_giant", "Giant Land Octopus", 42200020, Tier::Normal),
    e("skull_ant", "Skull Plate Giant Ant", 42810062, Tier::Normal),
    e("pumpkin_head_thin", "Thin Mad Pumpkin Head", 43411000, Tier::Normal),
    e("giant_crayfish", "Giant Crayfish", 44200035, Tier::Normal),
    e("flame_chariot", "Flame Chariot", 44600032, Tier::Hard),
    e("giant_dog", "Giant Dog", 45500040, Tier::Normal),
    e("giant_crow", "Giant Crow", 45600040, Tier::Normal),
    e("giant_wormface", "Giant Wormface", 45801072, Tier::Hard),
    e("small_flying_dragon", "Flying Dragon (Small)", 45050042, Tier::Hard),
    // --- Рядовые, добавленные 2026-08-23, второй заход ---
    // Всё, что осталось враждебного и адресуется по конвенции
    // `chr_id = npc_param_id / 10000`. Мимо неё прошли Lordsworn Soldier,
    // Lordsworn Knight, Foot Soldier, Giant Crayfish (c4421) и Mohg the Omen
    // (c4801): у них строки парама лежат в чужом диапазоне, а именами
    // Paramdex не подписаны - подтвердить нечем, поэтому не взяты.
    e("crab", "Crab", 22710012, Tier::Easy),
    e("black_crab", "Black Crab", 22730020, Tier::Easy),
    e("albinauric_crab", "Albinauric Crab", 22750024, Tier::Easy),
    e("albinauric_crab_giant", "Giant Albinauric Crab", 22740024, Tier::Normal),
    e("death_crab", "Death Crab", 22770020, Tier::Normal),
    e("frenzied_nomad", "Frenzied Nomad", 32010035, Tier::Easy),
    e("putrid_ancestral_shaman", "Putrid Ancestral Shaman", 33710062, Tier::Normal),
    e("white_wolf", "White Wolf", 40710012, Tier::Normal),
    e("azula_stray_small", "Farum Azula Stray", 41630072, Tier::Normal),
    e("bloodbane_stray_small", "Bloodbane Stray (small)", 41650030, Tier::Normal),
    e("rotten_stray_small", "Rotten Stray (small)", 41670040, Tier::Normal),
    e("glintstone_digger_large", "Large Glintstone Digger", 43840020, Tier::Normal),
    e("rotten_land_squirt", "Giant Rotten Land Squirt", 44420040, Tier::Easy),
    e("rotten_miranda_sprout", "Rotten Miranda Sprout", 44830040, Tier::Easy),
    e("bloodbane_crow", "Bloodbane Giant Crow", 45610068, Tier::Normal),
    e("bear", "Bear", 60310012, Tier::Hard),
    e("boar", "Boar", 60500012, Tier::Easy),
    // --- Мини-боссы ---
    e("troll", "Troll", 46001010, Tier::Boss),
    e("troll_knight", "Troll Knight", 46010020, Tier::Boss),
    e("snowfield_troll", "Snowfield Troll", 46020050, Tier::Boss),
    e("stonedigger_troll", "Stonedigger Troll", 46030910, Tier::Boss),
    e("golem", "Guardian Golem", 46600030, Tier::Boss),
    e("tree_spirit", "Ulcerated Tree Spirit", 46400000, Tier::Boss),
    e("tree_spirit_elite", "Tree Spirit (strong)", 46400950, Tier::Boss),
    e("erdtree_avatar", "Erdtree Avatar", 48100000, Tier::Boss),
    e("erdtree_avatar_elite", "Erdtree Avatar (strong)", 48100150, Tier::Boss),
    e("putrid_avatar", "Putrid Avatar", 48110040, Tier::Boss),
    e("runebear", "Runebear", 46300000, Tier::Boss),
    e("runebear_elite", "Runebear (strong)", 46300032, Tier::Boss),
    e("deathbird", "Deathbird", 49800000, Tier::Boss),
    e("deathbird_elite", "Deathbird (strong)", 49800020, Tier::Boss),
    e("crucible_knight", "Crucible Knight", 25000010, Tier::Boss),
    e("black_knife", "Black Knife Assassin", 21000034, Tier::Boss),
    e("bloodhound_knight", "Bloodhound Knight Darriwil", 42900010, Tier::Boss),
    e("nights_cavalry", "Nights Cavalry", 31500010, Tier::Boss),
    e("bell_bearing_hunter", "Bell Bearing Hunter", 31000010, Tier::Boss),
    e("tree_sentinel", "Tree Sentinel", 32510010, Tier::Boss),
    e("draconic_tree_sentinel", "Draconic Tree Sentinel", 32500033, Tier::Boss),
    e("loretta", "Loretta, Knight of the Haligtree", 32520054, Tier::Boss),
    e("red_wolf", "Red Wolf of Radagon", 31810020, Tier::Boss),
    e("godskin_apostle", "Godskin Apostle", 35600030, Tier::Boss),
    e("godskin_noble", "Godskin Noble", 35700028, Tier::Boss),
    e("leonine", "Leonine Misbegotten", 34600012, Tier::Boss),
    e("alabaster_lord", "Alabaster Lord", 36000010, Tier::Boss),
    e("cemetery_shade", "Cemetery Shade", 36640012, Tier::Boss),
    e("demihuman_queen", "Demi-Human Queen", 41300012, Tier::Boss),
    e("ancient_hero", "Ancient Hero of Zamor", 71000012, Tier::Boss),
    e("commander_niall", "Commander Niall", 30500051, Tier::Boss),
    e("magma_wyrm", "Magma Wyrm Makar", 49100026, Tier::Boss),
    e("theodorix", "Great Wyrm Theodorix", 49110052, Tier::Boss),
    e("tibia_mariner", "Tibia Mariner", 49500010, Tier::Boss),
    e("gargoyle", "Valiant Gargoyle", 47700033, Tier::Boss),
    e("omenkiller", "Omenkiller", 48200100, Tier::Boss),
    e("grafted_scion", "Grafted Scion", 46900007, Tier::Boss),
    e("fallingstar_beast", "Fallingstar Beast", 46800032, Tier::Boss),
    e("ancestor_spirit", "Regal Ancestor Spirit", 46700065, Tier::Boss),
    e("dragonkin", "Dragonkin Soldier", 46500060, Tier::Boss),
    // --- Драконы ---
    e("agheel", "Flying Dragon Agheel", 45000010, Tier::Boss),
    e("ekzykes", "Decaying Ekzykes", 45010040, Tier::Boss),
    e("glintstone_dragon", "Glintstone Dragon", 45020022, Tier::Boss),
    e("borealis", "Borealis the Freezing Fog", 45030050, Tier::Boss),
    e("ancient_dragon", "Ancient Dragon", 45100072, Tier::Boss),
    e("fortissax", "Lichdragon Fortissax", 45110066, Tier::Boss),
    e("greyoll", "Elder Dragon Greyoll", 45040042, Tier::Boss),
    // --- Легендарные боссы ---
    e("margit", "Margit", 21300014, Tier::Boss),
    e("godrick", "Godrick the Grafted", 47500014, Tier::Boss),
    e("rennala", "Rennala", 20310024, Tier::Boss),
    e("radahn", "Starscourge Radahn", 47300040, Tier::Boss),
    e("rykard", "God-Devouring Serpent", 47100038, Tier::Boss),
    e("astel", "Astel, Naturalborn of the Void", 46200062, Tier::Boss),
    e("morgott", "Morgott", 21300534, Tier::Boss),
    e("fire_giant", "Fire Giant", 47600050, Tier::Boss),
    e("mohg", "Mohg, Lord of Blood", 48000068, Tier::Boss),
    e("maliketh", "Maliketh", 21101072, Tier::Boss),
    e("godfrey", "Godfrey", 47200070, Tier::Boss),
    e("hoarah_loux", "Hoarah Loux, Warrior", 47210070, Tier::Boss),
    e("malenia", "Malenia", 21200056, Tier::Boss),
    e("radagon", "Radagon", 21900078, Tier::Boss),
    e("placidusax", "Placidusax", 45200000, Tier::Boss),
    // Синего свечения ему не вешаем - см. `NO_GLOW`.
    e("elden_beast", "Elden Beast", 22000078, Tier::Boss),
    // Двойник игрока. Своей модели у него нет - тело собирается из снаряжения
    // и внешности, и обычным путём такое не создать (проверено живьём,
    // четыре захода). Через свой пепел собирается сама игра.
    ash("mimic_tear", "Mimic Tear", 207_000, Tier::Boss),
    // Слеза-босс из Нокрона. Числа сняты с её ванильного размещения:
    // строка парама, поведение и внешность. Собирается только системой
    // призыва - см. `SpawnEntry::init`.
    pm("mimic_boss", "Mimic Tear (boss)", 526_100_965, 90_603_100, 24_020, Tier::Boss),
];

/// Спросить у самой игры, существуют ли строки парама у всей таблицы.
///
/// Paramdex отвечает на этот вопрос по данным другой версии игры, а живой
/// репозиторий - по той, что запущена. После патча это разные ответы, и
/// доверять стоит второму (правило про Paramdex Names: годится подтвердить,
/// не годится опровергнуть).
///
/// Заодно видно, у скольких строк вообще нашёлся ИИ: нулевой think - это
/// «появился и стоит столбом», и на экране это неотличимо от сломанного
/// спавна.
pub fn audit_table() -> String {
    use crate::i18n::t;
    let Ok(repo) = (unsafe { SoloParamRepository::instance() }) else {
        return t("params not loaded").to_string();
    };
    // Семьи, у которых есть хоть одна строка поведения - ОДНИМ проходом по
    // параму. Спрашивать `think_of_chr_family` на каждую строку значило бы
    // пройти по нему 400+ раз подряд, а он большой.
    let mut families: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for (id, _) in repo.rows::<NpcThinkParam>() {
        families.insert(id / 10_000);
    }
    let mut missing: Vec<&str> = Vec::new();
    let mut mindless: Vec<&str> = Vec::new();
    for e in SPAWN_TABLE {
        if u32::try_from(e.npc_param_id).ok().and_then(|id| repo.get::<NpcParam>(id)).is_none() {
            missing.push(e.key);
            continue;
        }
        // Своя семья или семья базовой модели - тот же порядок, что и у
        // `think_of_chr_family`, только по готовому множеству.
        let chr = e.chr_id().max(0) as u32;
        if !families.contains(&chr) && !families.contains(&(chr / 10 * 10)) {
            mindless.push(e.key);
        }
    }
    let mut out = format!("{}: {}", t("table checked against the running game"), SPAWN_TABLE.len());
    if !missing.is_empty() {
        out.push_str(&format!(" | {}: {}", t("no param row"), missing.join(", ")));
    }
    if !mindless.is_empty() {
        out.push_str(&format!(" | {}: {}", t("no AI row"), mindless.join(", ")));
    }
    out
}

pub fn entry(key: &str) -> Option<&'static SpawnEntry> {
    SPAWN_TABLE.iter().find(|e| e.key == key)
}

/// Награда «случайный враг»: конкретное существо выбирается в момент покупки,
/// а не при настройке.
///
/// Ступень (`Option<Tier>`) сужает выбор: `None` - кто угодно из таблицы.
/// Хранится отдельной таблицей, а не строками `SPAWN_TABLE` с нулевым
/// param-id: у псевдо-строки нет ни модели, ни ИИ, и любая проверка формы
/// id (`spawn_table_ids_are_well_formed`) на ней бы падала.
pub struct RandomPick {
    pub key: &'static str,
    /// Английское название, оно же ключ перевода (см. `i18n`).
    pub label: &'static str,
    pub tier: Option<Tier>,
}

const fn r(key: &'static str, label: &'static str, tier: Option<Tier>) -> RandomPick {
    RandomPick { key, label, tier }
}

pub const RANDOM_PICKS: &[RandomPick] = &[
    r("random_any", "Random enemy (I-IV)", None),
    r("random_easy", "Random: easy (I)", Some(Tier::Easy)),
    r("random_normal", "Random: normal (II)", Some(Tier::Normal)),
    r("random_hard", "Random: hard (III)", Some(Tier::Hard)),
    r("random_boss", "Random boss (IV)", Some(Tier::Boss)),
];

pub fn random_pick(key: &str) -> Option<&'static RandomPick> {
    RANDOM_PICKS.iter().find(|p| p.key == key)
}

/// То же самое, но отдаёт именно `&'static str` ключа - нужен `decode()` в
/// `rewards.rs`, чтобы получить владеющий `'static` кусок для `Copy`-варианта
/// `Action::SpawnEnemy`, не аллоцируя `String`.
pub fn key_of(name: &str) -> Option<&'static str> {
    entry(name).map(|e| e.key).or_else(|| random_pick(name).map(|p| p.key))
}

pub fn label(key: &str) -> String {
    if let Some(e) = entry(key) {
        return crate::i18n::t(e.label).to_string();
    }
    match random_pick(key) {
        Some(p) => crate::i18n::t(p.label).to_string(),
        None => key.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Случайная позиция в кольце вокруг игрока
// ---------------------------------------------------------------------------

/// Нижняя граница кольца - доля от радиуса, а не отдельный слайдер: просьба
/// пользователя была настраивать только сам радиус, а не садиться врагу прямо
/// в игрока.
const MIN_DIST_FRACTION: f32 = 0.3;

/// Xorshift32 вместо зависимости `rand` - формула на пять строк, крейт ради
/// нескольких случайных метров ни к чему (тот же довод, что у своего
/// .ini-парсера вместо serde). Не крипто-стойкий, позиции спавна это не нужно.
pub(crate) fn xorshift32(state: &mut u32) -> u32 {
    if *state == 0 {
        *state = 0x9E37_79B9; // 0 - неподвижная точка xorshift, уводим с неё
    }
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn unit_f32(state: &mut u32) -> f32 {
    xorshift32(state) as f32 / u32::MAX as f32
}

/// Смещение (dx, dz) в горизонтальной плоскости - высоту (Y) не трогаем,
/// движок сам решает, на какой высоте окажется модель. Равномерно по ПЛОЩАДИ
/// кольца `[radius*MIN_DIST_FRACTION, radius]`, не по радиусу - иначе точки
/// сгущались бы у внутренней границы.
pub fn random_offset(radius_m: f32, state: &mut u32) -> (f32, f32) {
    let radius_m = radius_m.max(0.1);
    let min_r = radius_m * MIN_DIST_FRACTION;
    let r = (min_r * min_r + unit_f32(state) * (radius_m * radius_m - min_r * min_r)).sqrt();
    let angle = unit_f32(state) * std::f32::consts::TAU;
    (r * angle.cos(), r * angle.sin())
}

/// Разброс по углу вокруг направления взгляда, радианы (примерно 12 градусов).
/// Ноль означал бы, что две покупки подряд ложатся ровно одна в другую.
const FRONT_SPREAD: f32 = 0.21;

/// Смещение (dx, dz) перед взглядом на расстояние `dist`.
///
/// `forward` - вектор камеры как есть, включая наклон вверх-вниз; здесь от
/// него берутся только X и Z. Смотреть в небо или под ноги не должно уносить
/// врага вверх или вниз: высоту всё равно определяет земля под точкой.
///
/// `None` - горизонтальной составляющей нет вовсе (взгляд строго вертикально),
/// и направление «вперёд» тогда не определено.
pub fn front_offset(forward: (f32, f32), dist: f32, state: &mut u32) -> Option<(f32, f32)> {
    let len = (forward.0 * forward.0 + forward.1 * forward.1).sqrt();
    if len < 1e-3 {
        return None;
    }
    let angle = forward.0.atan2(forward.1) + (unit_f32(state) - 0.5) * 2.0 * FRONT_SPREAD;
    Some((dist * angle.sin(), dist * angle.cos()))
}

/// Есть ли место под ещё один спавн. Чистое правило без обращения к игре:
/// заявка в полёте (`awaiting`) тоже занимает слот, иначе быстрые повторные
/// покупки проскочили бы лимит, пока движок ещё не создал предыдущего врага.
pub fn slot_available(active: usize, awaiting: bool, limit: u32) -> bool {
    active + usize::from(awaiting) < limit.max(1) as usize
}

/// Свободен ли ЭТОТ враг для розыгрыша «случайного».
///
/// Настройки «сколько одинаковых» больше нет (удалена по запросу 2026-09-07):
/// явный выбор стримера ничем не ограничен, а лотерея по-прежнему не
/// повторяется - тот же враг во второй раз не выпадет, пока первый в мире.
fn free_for_roll(same: usize) -> bool {
    same == 0
}

// ---------------------------------------------------------------------------
// Куда именно ставить: не в стену и не под землю
// ---------------------------------------------------------------------------

/// Сколько точек в кольце пробуем, прежде чем сдаться и поставить как выйдет.
///
/// Восемь - потому что дешевле десятка лучей в редком событии нет ничего, а
/// сотня попыток на закрытом балконе всё равно ничего не нашла бы.
const PLACEMENT_TRIES: u32 = 8;

/// На какой высоте идёт луч «видно ли туда» - примерно грудь игрока. От земли
/// он цеплялся бы за каждый бугор, от макушки - пролетал бы над перилами.
const EYE_HEIGHT: f32 = 1.2;

/// Насколько выше кандидата начинается луч поиска земли и как глубоко он идёт.
/// Вверх - чтобы начать заведомо над полом на склоне, вниз - чтобы поймать
/// обрыв и отбросить точку, висящую над пропастью.
/// Насколько земля под точкой спавна может отличаться по высоте от земли под
/// игроком. Ниже - это уже дно ямы или обрыва, выше - верх стены.
///
/// **Из-за отсутствия этого предела враги и оказывались «над ямой»** (жалоба
/// живьём 2026-08-23). Луч искал землю на двенадцать метров вниз и находил -
/// дно ямы. Формально земля есть, точка проходила проверку, и враг ставился на
/// дно, куда игроку не добраться. Проверять надо было не «есть ли внизу
/// что-нибудь», а «есть ли пол на той же высоте, что и под ногами».
///
/// Четыре метра на восемь метров в сторону - это уклон под 26 градусов: холмы
/// проходят, ямы нет.
const GROUND_REACH: f32 = 4.0;

/// Отступ от края обрыва.
///
/// Пол под самой точкой есть и у точки НА КРАЮ ямы - проверку она проходит
/// честно, а враг встаёт в паре пикселей от пропасти и падает от первого же
/// удара (жалоба живьём 2026-08-23). Поэтому пол проверяется ещё и на этом
/// расстоянии ДАЛЬШЕ от игрока: нет его там - значит мы на краю, и точку надо
/// брать ближе.
///
/// Шаг между кандидатами при радиусе 8 м - примерно те же 0.7 м, так что
/// следующий кандидат как раз и окажется на нужном отступе.
const EDGE_MARGIN: f32 = 0.75;

/// Насколько широкую площадку требуем вокруг точки призыва, метры.
///
/// Отладочный спавн ставит существо ровно в проверенную точку, а пепел - вокруг
/// неё, по смещениям своих строк: гость оказывается в нескольких метрах в
/// стороне, где пола может уже не быть. Отсюда и разница, замеченная живьём
/// 2026-09-08: через дебаг под землёй не появлялся никто, через пепел -
/// появлялись.
const SPREAD_CHECK: f32 = 4.0;

/// Маска фильтра лучей. Что в ней значит каждый бит, неизвестно, а значение
/// подбирается САМО: пускаем пробный луч вниз из-под ног игрока (он-то на
/// земле стоит заведомо) и оставляем первую маску, которая эту землю нашла.
///
/// Подбор дешевле ползунка в настройках: настраивать его вслепую всё равно
/// некому, а не найдись ни одна - проверка геометрии просто выключается, и
/// спавн работает как раньше.
const FILTERS: &[u32] = &[1, 2, 4, 8, 0x10, 0x20, 0x1000, 0xFFFF_FFFF];

/// Подобранная маска, `NOT_TRIED` - ещё не подбирали.
///
/// Неудачу НЕ запоминаем: подбор мог не удаться потому, что в тот момент
/// игрок висел в прыжке или на Торренте, а не потому, что лучи недоступны.
/// Запомненный отказ выключил бы проверку геометрии до конца сессии; восемь
/// лишних лучей на редкую покупку не стоят ничего.
static RAY_FILTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(NOT_TRIED);
const NOT_TRIED: u32 = u32::MAX;

fn cast(phys: &CSPhysWorld, filter: u32, from: HavokPosition, d: PositionDelta, owner: &PlayerIns) -> Option<HavokPosition> {
    phys.cast_ray(filter, &from, d, owner)
}

/// Высота пола под точкой - на той же высоте, что и под игроком.
///
/// `None` значит «пола на этой высоте нет»: обрыв, яма, вода, незагруженный
/// кусок карты или, наоборот, точка под нависающей стеной. Луч намеренно
/// короткий (`2 * GROUND_REACH`) - длинный находил бы дно ямы и считал его
/// годным полом.
fn ground_at(phys: &CSPhysWorld, filter: u32, x: f32, y: f32, z: f32, owner: &PlayerIns) -> Option<f32> {
    let from = HavokPosition(x, y + GROUND_REACH, z, 0.0);
    let hit = cast(phys, filter, from, PositionDelta(0.0, -2.0 * GROUND_REACH, 0.0), owner)?;
    Some(hit.1)
}

/// Видно ли из точки игрока в точку спавна по прямой. Стена между ними - это
/// как раз тот случай, ради которого всё и заводилось: враг за ней окажется в
/// соседней комнате или внутри скалы.
fn clear_line(phys: &CSPhysWorld, filter: u32, from: HavokPosition, to: HavokPosition, owner: &PlayerIns) -> bool {
    let d = PositionDelta(to.0 - from.0, to.1 - from.1, to.2 - from.2);
    cast(phys, filter, from, d, owner).is_none()
}

/// Луч в пустоту: пять сантиметров вверх от груди игрока. Там не может быть
/// ничего - ни пола, ни стены, ни его самого (он идёт `owner`, то есть
/// исключён).
///
/// **Маска, которая попадает и здесь, бьёт по всему подряд.** С ней
/// `clear_line` всегда отвечает «не видно», ни одна точка кольца не проходит
/// проверку, и спавн скатывается в запасной вариант - то есть ровно в стены и
/// над ямами, от которых всё затевалось (жалоба живьём 2026-08-23). Отличить
/// такую маску от рабочей по «нашлась ли земля» нельзя: земля с ней тоже
/// находится, и подбор считал её годной.
fn hits_nothing(phys: &CSPhysWorld, filter: u32, player: HavokPosition, owner: &PlayerIns) -> bool {
    let chest = HavokPosition(player.0, player.1 + EYE_HEIGHT, player.2, 0.0);
    cast(phys, filter, chest, PositionDelta(0.0, 0.05, 0.0), owner).is_none()
}

/// Маска, которой лучи в этом мире реально что-то находят.
fn ray_filter(phys: &CSPhysWorld, player: HavokPosition, owner: &PlayerIns) -> Option<u32> {
    if rays_off() {
        return None;
    }
    let cached = RAY_FILTER.load(std::sync::atomic::Ordering::Relaxed);
    if cached != NOT_TRIED {
        return Some(cached);
    }
    for &f in FILTERS {
        // Два условия, и оба обязательны: под ногами обязан найтись пол, а
        // луч в пустоту обязан вернуться ни с чем. «Близко» проверять больше
        // не нужно - луч и так не длиннее `GROUND_REACH` в каждую сторону.
        let grounded = ground_at(phys, f, player.0, player.1, player.2, owner).is_some();
        if grounded && hits_nothing(phys, f, player, owner) {
            RAY_FILTER.store(f, std::sync::atomic::Ordering::Relaxed);
            return Some(f);
        }
    }
    None
}


/// Точка, отодвинутая от игрока ещё на `extra` метров по той же прямой.
fn push_out(player: HavokPosition, x: f32, z: f32, extra: f32) -> (f32, f32) {
    let (dx, dz) = (x - player.0, z - player.2);
    let len = (dx * dx + dz * dz).sqrt();
    if len < 0.01 {
        return (x, z);
    }
    let k = (len + extra) / len;
    (player.0 + dx * k, player.2 + dz * k)
}

/// Точка спавна: случайная в кольце, но не за стеной и не в воздухе.
///
/// Без лучей (или когда ни одна маска не подошла) ведёт себя ровно как
/// раньше - смещение в кольце и высота игрока: пусть криво, но покупка
/// исполняется.
fn pick_position(
    world: &WorldChrMan,
    player: HavokPosition,
    radius_m: f32,
    in_front: bool,
    rng: &mut u32,
) -> HavokPosition {
    // Куда смотрит камера. Взгляд, а не разворот модели: зритель платит за то,
    // что увидит в кадре. Камеры нет - остаётся кольцо.
    let forward = in_front
        .then(|| unsafe { CSCamera::instance() }.ok())
        .flatten()
        .map(|cam| (cam.pers_cam_1.matrix.2 .0, cam.pers_cam_1.matrix.2 .2));

    // Кандидат номер `i` из `PLACEMENT_TRIES`. Перед взглядом они идут по
    // прямой, подступая всё ближе: упёрлись в стену в четырёх метрах - враг
    // встанет перед ней, а не за ней. Без направления - случайные точки кольца.
    let candidate = |dir: Option<(f32, f32)>, i: u32, rng: &mut u32| {
        let shrink = 1.0 - (i as f32 / PLACEMENT_TRIES as f32) * (1.0 - MIN_DIST_FRACTION);
        dir.and_then(|f| front_offset(f, radius_m.max(0.1) * shrink, rng))
            .unwrap_or_else(|| random_offset(radius_m, rng))
    };

    // Лучей нет вовсе - ставим как ставили до них: смещение и высота игрока.
    let fallback = |rng: &mut u32| {
        let (dx, dz) = candidate(forward, 0, rng);
        HavokPosition(player.0 + dx, player.1, player.2 + dz, 0.0)
    };
    let (Ok(havok), Some(owner)) = (unsafe { CSHavokMan::instance() }, world.main_player.as_ref()) else {
        return fallback(rng);
    };
    let phys = &*havok.phys_world;
    let Some(filter) = ray_filter(phys, player, owner) else {
        return fallback(rng);
    };
    let eye = HavokPosition(player.0, player.1 + EYE_HEIGHT, player.2, 0.0);
    // Два прохода: сперва перед взглядом, потом кольцом вокруг. Второй нужен
    // ровно для случая «игрок смотрит в яму»: там ВСЯ прямая перед ним
    // непригодна, и раньше это кончалось запасным вариантом, который ставил
    // врага вслепую - то есть в ту самую яму (жалоба живьём 2026-08-23).
    // Смотреть по сторонам дешевле, чем сдаваться.
    for dir in [forward, None] {
        for i in 0..PLACEMENT_TRIES {
            let (dx, dz) = candidate(dir, i, rng);
            let (x, z) = (player.0 + dx, player.2 + dz);
            if !clear_line(phys, filter, eye, HavokPosition(x, player.1 + EYE_HEIGHT, z, 0.0), owner) {
                continue;
            }
            // Пола на высоте игрока нет - там яма, обрыв, вода или нависающая
            // стена. Точку пропускаем.
            let Some(y) = ground_at(phys, filter, x, player.1, z, owner) else {
                continue;
            };
            // Тот же вопрос, но на `EDGE_MARGIN` дальше от игрока: под самим
            // краем обрыва пол есть, и без этой проверки враг встаёт на нём.
            let (ex, ez) = push_out(player, x, z, EDGE_MARGIN);
            if ground_at(phys, filter, ex, player.1, ez, owner).is_none() {
                continue;
            }
            // И площадка вокруг: пепел ставит своих не в точку, а вокруг неё.
            if !room_around(phys, filter, x, player.1, z, owner) {
                continue;
            }
            return HavokPosition(x, y, z, 0.0);
        }
        if dir.is_none() {
            break;
        }
    }
    // Не подошло ничего: тесная комната, узкий мост, площадка над пропастью.
    // Единственная точка, про которую мы ТОЧНО знаем, что там есть пол, - под
    // самим игроком. Неудобно, зато не в яме и не в стене; движок растолкает
    // их сам.
    HavokPosition(player.0, player.1, player.2, 0.0)
}

// ---------------------------------------------------------------------------
// Состояние + обвязка над живой памятью игры (без юнит-тестов, см. шапку)
// ---------------------------------------------------------------------------

/// `NpcThinkParam` для этого врага - его ИИ: кого замечать, когда нападать.
///
/// **Нулевой think и есть «появился, но не атакует»** (жалоба 2026-08-20):
/// без строки поведения существо просто стоит. Поэтому номер не угадывается, а
/// добывается по порядку надёжности:
///
/// 1. **У живого сородича в мире.** `EnemyIns::npc_think_param` - это ровно то
///    число, с которым игра сама запустила такого же врага. Дороже всего
///    (обход наборов), но спавн - редкое событие, а ответ точный.
/// 2. **По конвенции FromSoftware** номер совпадает с `NpcParam`. Часто верно,
///    но не гарантия, поэтому строка проверяется по параму: несуществующая -
///    это краш.
/// 3. **0** - враг будет стоять столбом. Плохо, но несравнимо лучше вылета
///    посреди стрима.
fn think_param_id(world: &WorldChrMan, repo: &SoloParamRepository, npc_param_id: i32) -> i32 {
    if let Some(think) = think_of_living_kin(world, npc_param_id) {
        return think;
    }
    if let Ok(id) = u32::try_from(npc_param_id) {
        if repo.get::<NpcThinkParam>(id).is_some() {
            return npc_param_id;
        }
    }
    think_of_chr_family(repo, npc_param_id).unwrap_or(0)
}

/// Строка поведения из «семьи» того же существа.
///
/// **Это и чинит стоящих столбом боссов** (жалоба 2026-08-20: Маления, Радан,
/// Годфри, Маликет, Реннала, рунный медведь появлялись и ничего не делали).
/// Сородича в мире у босса нет по определению - он один такой, - а его
/// `NpcThinkParam` номером с `NpcParam` не совпадает, и обе прежние попытки
/// давали ноль, то есть отсутствие ИИ.
///
/// Номера строк поведения лежат в том же диапазоне, что и сам персонаж:
/// `c2120` (Маления) -> `21200000..21209999`. Берём ближайшую к `npc_param_id`
/// существующую - у вариантов одного существа они идут рядом.
fn think_of_chr_family(repo: &SoloParamRepository, npc_param_id: i32) -> Option<i32> {
    let chr_id = npc_param_id / 10_000;
    // Своя семья, а если её нет - семья БАЗОВОЙ модели: у вариантов существа
    // (`c4071` белый волк при `c4070` волке) собственных строк поведения не
    // заводят вовсе, они ходят с ИИ базового. Без этого шага такой враг
    // получал think 0, то есть появлялся и стоял столбом (жалоба живьём
    // 2026-08-23 про белого волка). Проверено по `NpcThinkParam`: пусты
    // семьи c4071, c4281, c3471, c4442 и c4911, и у каждой базовая семья
    // строки имеет.
    family_think(repo, chr_id, npc_param_id).or_else(|| family_think(repo, chr_id / 10 * 10, npc_param_id))
}

/// Ближайшая к `npc_param_id` строка поведения в диапазоне модели `chr_id`.
fn family_think(repo: &SoloParamRepository, chr_id: i32, npc_param_id: i32) -> Option<i32> {
    let family = u32::try_from(chr_id).ok()? * 10_000;
    repo.rows::<NpcThinkParam>()
        .map(|(id, _)| id)
        .filter(|id| (family..family + 10_000).contains(id))
        .min_by_key(|id| (i64::from(*id) - i64::from(npc_param_id)).abs())
        .and_then(|id| i32::try_from(id).ok())
}

/// ИИ уже стоящего в мире врага того же вида.
///
/// Обход тот же, что у никнеймов: `open_field_chr_set` плюс `chr_sets` - мобы
/// легаси-подземелий в первый не входят. Зовётся только из `request`, то есть
/// раз на покупку, а не каждый кадр.
fn think_of_living_kin(world: &WorldChrMan, npc_param_id: i32) -> Option<i32> {
    let open = world.open_field_chr_set.base.characters();
    let rest = world.chr_sets.iter().flatten().flat_map(|s| s.characters());
    open.chain(rest)
        .filter(|c| c.npc_param_id == npc_param_id)
        .find_map(|c| c.as_subclass::<EnemyIns>().map(|e| e.npc_think_param))
        .filter(|think| *think > 0)
}

/// Сколько ждём появления `last_created_chr` после запроса, прежде чем
/// считать спавн не случившимся и вернуть баллы. Доккомент крейта обещает
/// "на следующем такте", но точный кадр не проверен живьём - берём с запасом.
const AWAIT_TIMEOUT: Duration = Duration::from_secs(2);

/// То же для союзника. Дольше: его собирает не отладочный создатель, а система
/// призыва - со своей анимацией появления и своим кулдауном на предмет.
const ALLY_AWAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Сколько существо «осваивается», прежде чем мод его судит.
///
/// Движок выдаёт указатель раньше, чем существо доинициализировано: на первых
/// кадрах его может не быть в `chr_sets`, а `hp` может быть ещё нулевым. Без
/// этой отсрочки мод счёл бы врага мёртвым сразу же, забыл про него - и тот
/// остался бы в мире навсегда, без TTL и без учёта в лимите.
///
/// Всё это время его будят каждый кадр, а по её истечении - один раз проверяют
/// (`took`) и решают: подтвердить покупку или убрать существо и вернуть баллы.
/// Четыре секунды, а не две: судить раньше, чем движок дособрал крупную
/// модель, значит отменять удачные спавны.
const SETTLE_GRACE: Duration = Duration::from_secs(4);

/// Сколько свеча горит после спавна. Дольше `SETTLE_GRACE`: движок собирает
/// существо ещё какое-то время после того, как отдал указатель, и вешается он
/// в том числе там. Цена - ложное обвинение, если игру закрыли руками в это
/// окно; снимается кнопкой «Простить».
const CANDLE_LINGER: Duration = Duration::from_secs(20);


pub enum SpawnRejected {
    LimitReached,
    /// Лотерея «случайного врага» не нашла никого, кого ещё нет в мире.
    SameLimitReached,
    UnknownEnemy,
    UnknownNpcParam,
    /// Движок ещё не обработал предыдущую заявку. Второй вызов затёр бы её
    /// `init_data` - см. `request`.
    Busy,
    /// Идёт бой с боссом, а стример это запретил (`spawn_block_in_boss`).
    BossFight,
    /// Игрока в мире нет - относительно чего ставить кольцо, неизвестно.
    NoPlayer,
    /// Поле `debug_chr_creator` не похоже само на себя: игру пропатчили, а
    /// раскладку структур в крейте - нет. Спавн выключен, пока не обновим.
    EngineMoved,
}

impl SpawnRejected {
    /// Почему не получилось - человеку в окно настроек. Без этого кнопка
    /// «Проверить» на незаполненной таблице молчит, и понять, что произошло,
    /// нельзя ничем: лога в моде нет.
    pub fn reason(&self) -> String {
        use crate::i18n::t;
        match self {
            SpawnRejected::LimitReached => {
                t("The spawn limit is already reached.").into()
            }
            SpawnRejected::SameLimitReached => t("Everyone from that group is already in the world.")
            .into(),
            SpawnRejected::UnknownEnemy => {
                t("No such enemy in the list.").into()
            }
            SpawnRejected::UnknownNpcParam => t("This enemy has no param id picked yet (see SPAWN_TABLE).")
            .into(),
            SpawnRejected::Busy => {
                t("The previous spawn is still processing.").into()
            }
            SpawnRejected::BossFight => {
                t("Boss fight in progress - spawning is blocked.").into()
            }
            SpawnRejected::NoPlayer => {
                t("The player is not in the world yet.").into()
            }
            SpawnRejected::EngineMoved => {
                t("The game update moved the spawner - spawning is off until the mod catches up.").into()
            }
        }
    }
}

struct TrackedSpawn {
    /// Кто это по куратор-списку - им считается лимит одинаковых.
    key: &'static str,
    /// Чем расплатились. Держится до приговора (`SETTLE_GRACE`): пока не
    /// известно, получилось ли, покупку нельзя ни подтвердить, ни вернуть.
    reward_id: String,
    redemption_id: String,
    /// Приговор уже вынесен - дальше запись живёт обычной жизнью, по TTL.
    confirmed: bool,
    handle: FieldInsHandle,
    /// Адрес числом, а не `NonNull<ChrIns>`: hudhook требует от render loop
    /// `Send + Sync`, а сырой указатель их не имеет - та же порода
    /// ограничения, что и запрет хранить `FontId` (см. overlay.rs).
    /// Разыменовывать его мы всё равно не собираемся, только сверять.
    addr: usize,
    /// Когда движок отдал нам этого врага - от него считается `SETTLE_GRACE`.
    born: Instant,
    /// Какую команду поставить, когда движок его дособрал.
    team: u8,
    /// Призван на стороне игрока - см. `AwaitingSpawn::ally`.
    ally: bool,
    /// Пришёл системой пеплов праха, а не отладочным создателем. Такому команду
    /// и активацию ставит сама игра, и лезть туда - значит спорить с ней.
    summoned: bool,
    /// Сколько рун стоит это существо (`NpcParam::get_soul`). Снимается в
    /// приговоре, когда существо уже в мире: у призванного своим пеплом строки
    /// парама в заявке нет вовсе, а у самого существа она есть всегда.
    souls: u32,
    /// Показания для оверлея, снятые последним тиком.
    view: Option<AllyView>,
    /// Здоровье на момент последнего удара и когда он был - из них считается
    /// «хвост урона» на полоске. Хранится момент, а не текущее значение:
    /// показание тогда чистая функция времени, без накопления ошибки по кадрам
    /// и без дельты времени в `tick`.
    lag_from: f32,
    lag_at: Instant,
    /// Когда команда переписывалась в последний раз (только у союзника).
    last_team_write: Option<Instant>,
    /// Когда последний раз подтверждали эффекты пепла.
    last_spirit: Option<Instant>,
    expires_at: Instant,
    /// Ник того, кто его купил. Он и висит над врагом вместо случайного
    /// зрителя из общей ротации - зритель заплатил именно за это.
    viewer: String,
    /// Строка парама и строка поведения - чтобы пересобрать пепел этим же
    /// существом, не выясняя ИИ заново обходом всего мира.
    npc: i32,
    think: i32,
    /// Сколько здоровья вернуть после пересборки. `0` - новый, не трогаем.
    ///
    /// Пишется после окна сборки, а не при усыновлении: в нём любое касание
    /// существа трижды подряд вешало игру.
    restore_hp: i32,
    /// Куда просили поставить. Прах ставит гостя сам и мимо - см. `place_at`.
    anchor: HavokPosition,
    /// Гость уже стоит на своей точке и на земле. До этого расставляем каждый
    /// кадр: анимация материализации перетирает одну запись.
    placed: bool,
}

/// Кто должен оказаться в мире после пересборки пепла.
///
/// Активный пепел у игры один, и новый запрос убирает прошлый. Поэтому каждая
/// покупка пересобирает его целиком: прежние призванные плюс новый, до потолка
/// из настроек. Здоровье переносится, чтобы избитый враг не возвращался целым.
#[derive(Clone)]
struct AshMember {
    key: &'static str,
    /// Своя у каждого: перенесённому чужая команда досталась бы от того, кого
    /// покупают сейчас.
    team: u8,
    npc: i32,
    think: i32,
    /// Строка внешности у существа на модели игрока. `0` - обычное существо.
    init: i32,
    ally: bool,
    hp: i32,
    ttl: Duration,
    viewer: String,
    reward_id: String,
    redemption_id: String,
    /// Приговор по нему уже вынесен - второй раз покупку не подтверждаем.
    confirmed: bool,
    /// Куда его ставить. У новичка - точка, выбранная лучами, у перенесённого
    /// через пересборку - то место, где он стоял: иначе прежние сбегаются к
    /// новому, ведь пепел у всех один и точка призыва у него одна.
    at: HavokPosition,
}

/// Команда «враг» из таблицы альянсов игры. Запасной вариант, если у строки
/// парама она не проставлена: существо в нулевой команде никого не считает
/// противником и потому не нападает.
const TEAM_ENEMY: u8 = 6;

/// Как часто союзнику переписывается команда, пока он «свежий».
///
/// Одной записи в приговоре живьём не хватило: движок ставит существу свою
/// команду не только при создании, и Маления через несколько секунд снова
/// дралась с игроком (жалоба 2026-09-07).
const ALLY_REASSERT: Duration = Duration::from_millis(500);

/// Команда «дух-призыв» - та же, на которой сидят пепла праха и Ложная Слеза.
/// Проверена живьём 2026-09-07: призванный сразу становится союзником, ручку
/// подбора номера после этого убрали.
const TEAM_ALLY: u8 = 47;

/// Сколько после рождения союзнику вообще переписывается команда. Дальше он
/// либо уже на нашей стороне, либо не будет никогда, а лишние записи в живую
/// структуру каждые полсекунды до конца TTL - плата ни за что.
const ALLY_REASSERT_FOR: Duration = Duration::from_secs(30);

struct AwaitingSpawn {
    key: &'static str,
    reward_id: String,
    redemption_id: String,
    /// `last_created_chr` на момент запроса, адресом (0 - пусто). По нему
    /// отличаем "движок ещё не подхватил заявку" от "давно висит что-то чужое".
    previous_last_created: usize,
    /// Кто стоял в наборе пеплов праха на момент запроса. Непусто только у
    /// союзника: его создаёт система призыва, и `last_created_chr` она не
    /// трогает - своего опознаём по новому хэндлу в наборе.
    previous_buddies: Vec<FieldInsHandle>,
    /// Каким триггером звали систему призыва. `0` - шли обычным путём, через
    /// отладочный создатель.
    buddy_trigger: i32,
    /// Кого ждём от этого запроса. Пусто у отладочного пути.
    members: Vec<AshMember>,
    deadline: Instant,
}

/// Купленный спавн, который ждёт своего часа.
///
/// Задержка нужна ради зрелища: над карточкой покупки идёт отсчёт, стример
/// успевает добежать до удобного места, а зрители - посмотреть, как он это
/// делает. Без неё босс сваливается на голову в тот же кадр.
struct DelayedSpawn {
    key: &'static str,
    due: Instant,
    ttl: Duration,
    /// Призвать на своей стороне, а не врагом.
    ally: bool,
    viewer: String,
    reward_id: String,
    redemption_id: String,
}

// ---------------------------------------------------------------------------
// Кто уронил игру
// ---------------------------------------------------------------------------
//
// Спавн - единственное место в моде, после которого игра может закрыться
// целиком: движок собирает существо сам, и на некоторых он падает. Изнутри
// это не поймать ничем - процесс умирает вместе с окном настроек, и
// `note` вместе с ним.
//
// Поэтому «свеча»: ключ пишется в файл ПЕРЕД вызовом движка и стирается,
// когда существо признано состоявшимся (`took`, через `SETTLE_GRACE`). Файл,
// переживший запуск игры, и есть виновник - другого способа узнать его нет.

/// Правдоподобен ли `debug_chr_creator` - то есть не уехало ли его смещение
/// после патча игры.
///
/// `OwnedPtr` обещает не-null и врёт (то же самое уже ловили на
/// `main_player_game_data` в меню). Поле лежит далеко в структуре
/// `WorldChrMan`, за `main_player` и `chr_sets`: добавь FromSoft одно поле
/// перед ним - всё, что мод читает, работает как работало, а спавн начинает
/// писать `init_data` в чужую память. Симптом ровно такой: спавн роняет игру
/// КАЖДЫЙ раз, а остальное цело.
///
/// Проверяем двумя дешёвыми вопросами: указатель выглядит указателем, и по
/// нему лежит vftable внутри образа игры. У мусора оба совпадают редко.
fn creator_ok(world: &WorldChrMan) -> bool {
    let raw = unsafe { std::ptr::addr_of!(world.debug_chr_creator).cast::<usize>().read_unaligned() };
    if raw < 0x1_0000 || raw >= 0x7FFF_FFFF_FFFF || raw % 8 != 0 || !readable(raw) {
        return false;
    }
    let vftable = unsafe { (raw as *const usize).read_unaligned() };
    use pelite::pe64::PeObject;
    let image = fromsoftware_shared::program::Program::current().image();
    let base = image.as_ptr() as usize;
    (base..base + image.len()).contains(&vftable)
}

/// Отображена ли страница по этому адресу. Дешевле, чем поймать краш на
/// чтении: `VirtualQuery` - один системный вызов, а спавн редок.
pub(crate) fn readable(addr: usize) -> bool {
    use hudhook::windows::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery};
    let mut info = MEMORY_BASIC_INFORMATION::default();
    let n = unsafe { VirtualQuery(Some(addr as *const _), &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
    n != 0
        && info.State == MEM_COMMIT
        && (info.Protect & (PAGE_NOACCESS | PAGE_GUARD)).0 == 0
}

/// Файл-свеча рядом с DLL. Одна строка, живёт секунды.
const CRASH_FILE: &str = "game_information_counter.spawncrash";

static CRASH_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Кто уронил игру в прошлый запуск: из случайных такой больше не выпадает.
///
/// Живёт до конца сессии, а не в файле: файл при старте стирается, иначе
/// список копился бы навсегда, а ложное обвинение (закрыли игру руками в те
/// самые четыре секунды) снималось бы только вручную.
static BLAMED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// Лучи уронили игру в прошлый запуск - в этой сессии их не зовём.
static RAYS_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Свеча стирается при старте, а игру перезапускают раньше, чем идут смотреть
/// файл - поэтому прочитанное держим на виду до конца сессии.
static LAST_CANDLE: Mutex<Option<String>> = Mutex::new(None);

/// Прочитать свечу от прошлого запуска и запомнить, куда писать свою.
/// Зовётся один раз при загрузке мода - `dll_sibling` требует наш `HMODULE`.
pub fn init(dll: usize) {
    let Some(path) = crate::config::dll_sibling(dll, CRASH_FILE) else {
        return;
    };
    if let Ok(text) = std::fs::read_to_string(&path) {
        let (key, stage) = text.trim().split_once('|').unwrap_or((text.trim(), ""));
        *LAST_CANDLE.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.trim().to_string());
        match stage {
            // Лучи - вызов функции игры по RVA, единственный во всём моде.
            // Упали на нём - выключаем их на сессию: спавн вернётся к
            // поведению «кольцо без проверок», которое работало всегда.
            "rays" => RAYS_OFF.store(true, std::sync::atomic::Ordering::Relaxed),
            // Всё остальное зависит от самого существа: строки парама, ИИ,
            // сборка модели движком. Значит виноват враг, а не механика.
            _ => {
                if let Some(key) = entry(key).map(|e| e.key) {
                    blamed_mut().push(key);
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }
    *CRASH_PATH.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
}

/// Что было в свече от прошлого запуска: `ключ|этап`.
pub fn last_candle() -> Option<String> {
    LAST_CANDLE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Выключены ли лучи после падения на них. Только на эту сессию.
pub fn rays_off() -> bool {
    RAYS_OFF.load(std::sync::atomic::Ordering::Relaxed)
}

fn blamed_mut() -> std::sync::MutexGuard<'static, Vec<&'static str>> {
    BLAMED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Кого исключили из случайных после краша - строкой в окно настроек.
pub fn blamed() -> Vec<&'static str> {
    blamed_mut().clone()
}

/// Забыть обвинение - вдруг игру закрыли руками ровно в тот момент.
pub fn forgive() {
    blamed_mut().clear();
}

/// Зажечь свечу на этапе `stage`: «на чём именно упали» с экрана не видно
/// ничем, а этапов у спавна четыре, и лечатся они по-разному.
fn light_candle(key: &str, stage: &str) {
    if let Some(path) = CRASH_PATH.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = std::fs::write(path, format!("{key}|{stage}"));
    }
}

fn blow_out_candle() {
    if let Some(path) = CRASH_PATH.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = std::fs::remove_file(path);
    }
}

#[derive(Default)]
pub struct SpawnState {
    /// Чем кончилась последняя заявка - строкой в окно настроек.
    ///
    /// Лога в моде нет, а «появился невидимым» и «не появился вовсе» с экрана
    /// не различить: во втором случае смотреть просто не на что. Приговор
    /// (`took`) эту разницу знает точно, и глупо держать её в себе.
    note: Option<String>,
    /// Строки для отладочного окна: что мод делает с пеплом прямо сейчас.
    /// Собираются раз в `TEND_EVERY`, там же, где мод и без того обходит
    /// гостей.
    debug: Vec<String>,
    /// Замер расстановки последней заявки - туда же, в отладочное окно.
    place_note: String,
    tracked: Vec<TrackedSpawn>,
    awaiting: Option<AwaitingSpawn>,
    delayed: Vec<DelayedSpawn>,
    /// Ники купленных врагов, которых уже нет среди живых.
    ///
    /// Запись о спавне снимается в тот же кадр, когда `hp` дошёл до нуля -
    /// слот лимита обязан освободиться сразу. А игра полоску здоровья и тег
    /// над головой в этот момент ещё держит, и подпись на пару секунд
    /// доставалась случайному зрителю из общей ротации: на экране это
    /// выглядело как «ник покупателя подменили» (жалоба 2026-08-23).
    ghosts: Vec<(FieldInsHandle, String, Instant)>,
    /// Entity id какого-нибудь настоящего Монумента возрождения. Менеджеру он
    /// нужен как владелец группы; какой именно - неважно, монумента рядом всё
    /// равно нет.
    stone: u32,
    /// Строка поведения, с которой живёт союзник.
    ash_think: i32,
    /// Встал ли `is_buddy_ai` на неё. `9` - строку не нашли.
    buddy_ai: u32,
    /// Чем кончилась последняя заявка. Отдельно от `note`: строка призыва
    /// переписывается два раза в секунду и затирала приговор раньше, чем его
    /// успевали прочитать.
    verdict: Option<String>,
    /// Когда последний раз писали в систему призыва.
    last_tend: Option<Instant>,
    /// Подменённая строка `BuddyParam`, которую надо вернуть на место.
    ///
    /// Живёт у состояния, а не у заявки: заявку снимают в первом же кадре
    /// после появления существа, то есть раньше срока подмены, и строка
    /// осталась бы переписанной до конца сессии - собственный пепел игрока
    /// призывал бы нашего.
    buddy_row: Option<BuddyHijack>,
    /// Переписанная строка усиления пепла и её исходные множители. Держится,
    /// пока в мире есть Слеза, и возвращается, когда её не стало.
    doping_hold: Vec<(u32, Doping)>,
    /// Строки поведения, которым одолжен `is_buddy_ai`, и что там стояло.
    ///
    /// Держится ровно окно сборки существа, как и подменённая строка пепла.
    /// Дольше нельзя: строка ОБЩАЯ со всеми обычными врагами того же вида, и
    /// на буддийском ИИ они бегают все разом - живьём 2026-09-08 это уронило
    /// FPS вдвое.
    ///
    /// Строк несколько, потому что пересборка создаёт заново ВСЕХ, а не только
    /// того, кого купили: раньше буддийский ИИ доставался лишь текущей покупке,
    /// и прежний союзник возвращался с обычным (живьём 2026-09-09).
    ai_hold: Option<(Instant, Vec<(u32, u8)>)>,
    rng: u32,
}

/// Сколько ник покупателя держится над уже мёртвым врагом. Заведомо дольше,
/// чем игра показывает его полоску, - лишние секунды никому не видны, потому
/// что показывать их не на чем.
const GHOST_LINGER: Duration = Duration::from_secs(15);


/// Позиция игрока - откуда считать кольцо спавна.
pub fn player_position(world: &WorldChrMan) -> Option<HavokPosition> {
    Some(world.main_player.as_ref()?.chr_ins.modules.physics.position)
}

/// Тот ли это самый враг, и жив ли он ещё.
///
/// `FieldInsHandle` **не содержит поколения**, поэтому слот к этому моменту мог
/// достаться совсем другому существу. Адрес сверяется до всего остального: без
/// этой проверки мод выгружал бы чужого.
fn chr_of<'a>(world: &'a mut WorldChrMan, t: &TrackedSpawn) -> Option<&'a mut ChrIns> {
    match world.chr_ins_by_handle_mut(&t.handle) {
        Some(chr) if chr as *const ChrIns as usize == t.addr => Some(chr),
        _ => None,
    }
}

fn alive(world: &mut WorldChrMan, t: &TrackedSpawn) -> bool {
    chr_of(world, t).is_some_and(|chr| chr.modules.data.hp > 0)
}

/// Прицепилась ли к существу модель.
///
/// **Это и есть признак невидимого столба** (жалоба живьём 2026-08-23):
/// `spawn_debug_character` ассеты не грузит, и если модели `c####` в памяти
/// не оказалось, существо всё равно создаётся - с хитпоинтами, но без тела и
/// без поведения. Снаружи это выглядит как «покупка пропала».
///
/// `OwnedPtr` обещает не-null и врёт: ровно так же врёт
/// `main_player_game_data` в главном меню (см. «Порядок чтения синглтонов»).
/// Поэтому читаем сырым числом через `addr_of!`, не создавая ссылку на
/// возможный ноль, и не разыменовываем вовсе.
fn has_model(chr: &ChrIns) -> bool {
    let raw = unsafe { std::ptr::addr_of!(chr.chr_model_ins).cast::<usize>().read() };
    raw >= 0x1_0000 && raw < 0x7FFF_FFFF_FFFF
}

/// Сколько здоровья показывать «хвостом» через `since` после удара.
///
/// Держится `LAG_HOLD`, потом за `LAG_DRAIN` стекает к текущему - так это
/// выглядит у полосок самой игры.
fn lag_value(from: f32, hp: f32, since: Duration) -> f32 {
    let t = since.as_secs_f32();
    if t <= LAG_HOLD {
        return from;
    }
    let k = ((t - LAG_HOLD) / LAG_DRAIN).clamp(0.0, 1.0);
    from + (hp - from) * k
}

/// Сколько «хвост урона» стоит на месте, прежде чем стечь, и сколько стекает.
const LAG_HOLD: f32 = 0.45;
const LAG_DRAIN: f32 = 0.55;

/// Строка союзника для своего оверлея: кто прислал, кого прислал, сколько
/// здоровья и сколько осталось жить.
///
/// Своя отрисовка, а не список пеплов праха: писать в `spirit_ash_display`
/// пробовали живьём 2026-09-07 и на экране не появилось ничего - буфер
/// забирает scaleform раньше, чем мод доходит до `Present`.
#[derive(Clone, Debug, PartialEq)]
pub struct AllyView {
    pub viewer: String,
    pub name: String,
    pub hp: u32,
    pub max_hp: u32,
    /// Доля полоски под «хвост урона» - бледная часть позади красной, которая
    /// показывает, сколько сняли только что. Всегда не меньше доли здоровья.
    pub lag: f32,
    pub secs_left: f32,
}

/// Команда плюс разрешение на активацию - всё, что мод делает с союзником.
///
/// **Команда пишется поверх ЛЮБОЙ, а не только поверх нулевой** (как у врага):
/// движок оставляет существу его собственную, ненулевую, и проверка «только
/// поверх нулевой» до союзника не дошла бы никогда.
///
/// **И не один раз.** Живьём 2026-09-07 Маления, ставшая союзником, через
/// несколько секунд снова дралась с игроком: команду движок ставит не только
/// при создании. Отсюда повтор раз в `ALLY_REASSERT` первые
/// `ALLY_REASSERT_FOR` - дальше существо либо на нашей стороне, либо не будет
/// на ней никогда.
///
/// **Никакого «сна» у союзника нет, и это выяснено замером 2026-09-08.**
/// Диагностика показала `team 47, activation 1, think 43510000` - всё, что мод
/// пишет, ложится верно, - а рядом с врагом призванный кидается в бой сам.
/// То есть он не спит, а СТОИТ БЕЗ ЦЕЛИ: врагу на команде 6 игрок противник
/// сразу, а союзнику драться не с кем, пока рядом никого нет.
///
/// Отсюда и «будит телепорт»: он переносил союзника туда, где враги. Ни
/// телепорта, ни подмены ИИ, ни игры с битами для этого не нужно - чинить было
/// нечего.
///
/// `activation_enabled` - крейт пишет, что им управляет
/// `NPC_PARAM_ST::disableActivateOpen`, то есть «оживает ли существо в открытом
/// мире».
///
/// **Пользы от него живьём НЕ видно** (2026-09-08): союзник ведёт себя
/// одинаково с ним и без. Оставлен не за компанию, а ради боссов: в августе
/// замерено, что у них активация в открытом мире выключена парамом, а будить их
/// положено скриптом арены, которого при нашем спавне нет. Обычному врагу он
/// просто не нужен.
///
/// ponytail: это бифилд, а на бифилдах в 2.7 мод обжёгся дважды -
/// `ChrDebugFlags` вешал игру, `ChrCtrlFlags` делал союзника бессмертным.
/// Но `chr_activation_flags` лежит вплотную к `chr_flags1c5`/`chr_flags1c6`,
/// а те заведомо на своих местах: на них стоят снятие по TTL и запрет выдачи
/// рун, и оба работают живьём. `ChrDebugFlags` при этом НЕ трогаем - именно он
/// вешал игру трижды подряд.
fn tend_spawn(world: &mut WorldChrMan, t: &mut TrackedSpawn, now: Instant) {
    // Два срока, и они разные. Команду переписываем только первые полминуты -
    // дальше существо либо на нашей стороне, либо не будет на ней никогда.
    // Эффекты пепла подтверждаем ВСЮ жизнь: они истекают, а идти надо всё
    // время.
    let team_due = due_for_team_write(t.born, t.last_team_write, now);
    let spirit_due = due_every(t.last_spirit, now, SPIRIT_EVERY);
    // Пока гость не встал - каждый кадр; дальше проверяем, не улетел ли он,
    // заодно с эффектами. Резолвить существо чаще незачем: падение видно и
    // через две секунды, а лишний поиск по хэндлу каждый кадр мод уже
    // оплачивал однажды половиной FPS.
    if t.placed && !team_due && !spirit_due {
        return;
    }
    let team = t.team;
    let player = player_position(world);
    let Some(chr) = chr_of(world, t) else { return };
    if !has_model(chr) {
        return;
    }
    keep_placed(chr, t, player, now);
    if team_due {
        // Пишем всегда. Разбирать, «своё» ли значение поставила игра, не
        // вышло: враждебных команд у неё несколько (живьём 2026-09-08 приходили
        // и `6`, и `7`), а список их неизвестен. Гипотеза, что наши 47 мешают
        // ИИ пепла, проверена и не подтвердилась.
        chr.team_type = team;
        chr.chr_activation_flags.set_activation_enabled(true);
        t.last_team_write = Some(now);
    }
    if spirit_due {
        if t.ally {
            // Следование и синий вид - эффектами самой игры, а не полями
            // записи группы. Это и есть весь механизм следования.
            chr.apply_speffect(SPIRIT_FOLLOW, true);
            if NO_GLOW.contains(&t.key) {
                chr.remove_speffect(SPIRIT_GLOW);
            } else {
                chr.apply_speffect(SPIRIT_GLOW, true);
            }
        } else {
            // Врага привела та же система пеплов, и она повесила ему всё
            // причитающееся духу - в том числе синее свечение. Оно читается
            // как «этот на моей стороне», поэтому врагу снимаем.
            for e in SPIRIT_EFFECTS {
                chr.remove_speffect(e);
            }
        }
        t.last_spirit = Some(now);
    }
}

/// Поставить существо в точку.
///
/// Одной координаты мало: капсулу физики двигает отдельный флаг, а без
/// `last_update_position` движок посчитает по разнице скорость и швырнёт гостя.
/// Счётчик падения обнуляем: вернувшийся из пустоты не должен разбиться о
/// землю, до которой он летел.
fn place_at(chr: &mut ChrIns, at: HavokPosition) {
    let p = &mut chr.modules.physics;
    p.position = at;
    p.last_update_position = at;
    p.chr_proxy_pos_update_requested = true;
    chr.modules.fall.fall_timer = 0.0;
}

/// Держать гостя там, куда его просили, и вытаскивать из пустоты.
///
/// **Зачем это вообще.** Куда поставит гостя система пеплов, мод не решает:
/// поля высоты у строки пепла нет вовсе, точку призыва движок ищет сам, и на
/// крупных промахивается - те тонут или проваливаются под карту. Правил этого
/// поиска не знает никто (в парамах их нет, в открытых разборах игры тоже), а
/// три захода с поправкой к `spawn_origin` кончились ничем: призыв не смещает
/// существо на неё, а зеркалит вниз.
///
/// Поэтому место назначаем сами - теми же координатами и той же поправкой
/// `lift_of`, которыми давно и успешно пользуется отладочный создатель.
///
/// Три случая в одной функции, потому что вопрос один - «стоит ли он там, где
/// надо»:
///
/// - пока не встал - пишем каждый кадр: у анимации материализации своя
///   подвижка, и одну запись она перетрёт;
/// - встал - больше не трогаем вовсе;
/// - улетел ниже якоря на `FALL_LIMIT` - это пустота, а не уступ: возвращаем
///   на свежую точку у игрока.
fn keep_placed(
    chr: &mut ChrIns,
    t: &mut TrackedSpawn,
    player: Option<HavokPosition>,
    now: Instant,
) {
    let y = chr.modules.physics.position.1;
    if t.placed {
        // Пока гость на своих ногах, якорь идёт за ним: «упал» - это десять
        // метров вниз от того места, где он последний раз стоял, а не от точки
        // призыва. Иначе спуск с холма за игроком читался бы падением.
        if chr.modules.physics.standing_on_solid_ground {
            t.anchor = chr.modules.physics.position;
            return;
        }
        if !fell_below(t.anchor.1, y) {
            return;
        }
        // Точка, куда его ставили, может быть уже далеко позади - гость ходит.
        // Возвращаем к игроку: это то же самое, что игра делает с отставшим
        // пеплом сама.
        if let Some(p) = player {
            t.anchor = p;
        }
        t.placed = false;
    }
    let target = target_y(t.anchor, chr.modules.physics.hit_height);
    place_at(chr, target);
    // «Встал» - это опора под ногами, а не просто «прошло время». Срок нужен
    // только чтобы не писать вечно, если опоры не будет никогда.
    if chr.modules.physics.standing_on_solid_ground || now.duration_since(t.born) > PLACE_FOR {
        t.placed = true;
    }
}

/// Куда именно ставить: точка от лучей - это ПОЛ, а у крупных точка привязки
/// в середине туши, и без поправки они уходят по пояс в землю. Формула и
/// поправка те же, что у отладочного пути.
fn target_y(at: HavokPosition, hit_height: f32) -> HavokPosition {
    HavokPosition(at.0, at.1 + lift_of(hit_height), at.2, 0.0)
}

/// Провалился ли гость. Десять метров вниз - это уже не уступ и не склон.
fn fell_below(anchor_y: f32, y: f32) -> bool {
    y < anchor_y - FALL_LIMIT
}

const FALL_LIMIT: f32 = 10.0;

/// Сколько ждём опоры под ногами, прежде чем перестать расставлять.
///
/// Коротко намеренно. Записи нужны, чтобы пережить подвижку анимации
/// материализации, а не чтобы держать гостя в воздухе: крупному мы задираем
/// точку на `lift_of`, и целую секунду он висел бы на ней, вместо того чтобы
/// опуститься (живьём 2026-09-09: hh 10, встал на пол через секунду висения).
const PLACE_FOR: Duration = Duration::from_millis(150);

/// Призвать союзника СИСТЕМОЙ ПЕПЛОВ ПРАХА, а не отладочным создателем.
///
/// **Зачем отдельный путь.** Существо, созданное `spawn_debug_character` и
/// перекрашенное в команду духа, за игроком не ходит и ходить не будет: пепел
/// праха следует не из-за команды и не из-за строки поведения, а потому что его
/// ведёт система призыва. Ни подмена `NpcThinkParam` (2026-09-07), ни телепорт,
/// ни игры с битами этого не заменили - следование живёт в подсистеме, к
/// которой мы просто не обращались.
///
/// **Как.** Пепел праха - это строка `BuddyParam`, в которой написано, КОГО
/// призывать (`npc_param_id`) и с каким ИИ (`npc_think_param_id`). Подменяем эти
/// два поля у одной строки, дёргаем её триггер - и движок сам собирает
/// настоящий призыв, со следованием и полоской в списке слева.
///
/// Это тот же приём, что уже выиграл с обзором камеры: править не результат, а
/// ПАРАМ, из которого движок его считает.
///
/// Строку возвращаем на место сразу, как движок её прочитал - иначе настоящие
/// пеплы игрока до конца сессии призывали бы не тех.
fn summon_via_ash(
    world: &mut WorldChrMan,
    repo: &mut SoloParamRepository,
    own_ash: i32,
    members: &[AshMember],
    at: HavokPosition,
) -> Option<BuddyHijack> {
    // Ложная Слеза и подобные призываются СВОИМ триггером, а строки их блока
    // трогать нельзя вовсе - от правки игра падает.
    if own_ash != 0 {
        place_and_trigger(world, at, own_ash);
        return Some(BuddyHijack {
            rows: Vec::new(),
            trigger: own_ash,
            at: Instant::now(),
        });
    }
    let trigger = repo.get::<BuddyParam>(BUDDY_ROW)?.trigger_sp_effect_id();
    if trigger <= 0 {
        return None;
    }
    let mut rows = Vec::new();
    for i in 0..BUDDY_ROWS {
        let id = BUDDY_ROW + i;
        let Some(row) = repo.get_mut::<BuddyParam>(id) else { continue };
        rows.push(BuddyRowBackup {
            id,
            npc: row.npc_param_id(),
            think: row.npc_think_param_id(),
            follow: row.pc_follow_type(),
            init: row.npc_player_init_param_id(),
            anim: row.generate_anim_id(),
            share: row.disable_pc_target_share(),
            x: row.x_offset(),
            z: row.z_offset(),
        });
        let Some(m) = members.get(i as usize) else {
            // Лишняя строка блока: `-1` в «кого призывать» её отключает.
            row.set_npc_param_id(-1);
            continue;
        };
        row.set_npc_param_id(m.npc);
        // Двойник собирается из внешности и снаряжения, а не из ассета модели.
        // Обе строки для этого - его, и анимация материализации тоже: без неё
        // тело не появляется.
        row.set_npc_player_init_param_id(if m.init == 0 { -1 } else { m.init });
        if m.init != 0 {
            row.set_generate_anim_id(MIMIC_ANIM);
        }
        // Строка поведения - СВОЯ, существа. Пепловая проверена живьём
        // (2026-09-08) и не годится: с ней союзник поворачивался к игроку и
        // стоял. Скрипт поведения лежит в ассетах самого существа, и чужой
        // номер для него просто не существует - ИИ остаётся без целей вовсе.
        row.set_npc_think_param_id(m.think);
        // `0` - идти за игроком, `1` - стоять на месте. Подтверждено эталоном,
        // снятым с живого пепла. Врагу ходить за игроком не положено.
        row.set_pc_follow_type(u8::from(!m.ally));
        // Общая цель с игроком - это про помощника: он бьёт того, кого бьёт
        // игрок. Врагу её оставлять нельзя - строка блока пепловая, и с ней
        // враг махал в сторону, где никого нет (живьём 2026-09-09).
        row.set_disable_pc_target_share(u8::from(!m.ally));
        // Смещения обнуляем: пусть все встают в саму точку призыва.
        //
        // Пол мод проверяет лучом под точкой и вокруг неё, а родные смещения
        // уводят гостей на несколько метров в стороны - и там они проваливались
        // под землю. Через отладочный создатель, который ставит существо ровно
        // в заданные координаты, такого не бывало ни разу: разница ровно в
        // этом (живьём 2026-09-08). Подбрасывать и вытаскивать лучом пробовали
        // - лечит следствие, а гости после падения гибнут.
        //
        // Расставит их движок сам: в одну точку он никого не впечатывает.
        //
        // Вернуть каждому его прежнее место пробовали трижды (мировые оси, оси
        // взгляда, перевёрнутый знак) - все три раза мимо: союзник уходит с
        // места раньше, чем пепел соберётся, и «прежнее место» устаревает.
        row.set_x_offset(0.0);
        row.set_z_offset(0.0);
    }
    if rows.is_empty() {
        return None;
    }
    // Больше в строках НИЧЕГО не трогаем.
    //
    // Обнуление `generate_anim_id`, `appear_on_around_sekihi`, верховых полей и
    // смещений прожило одну сборку: после него призванные перестали появляться
    // вовсе (живьём 2026-09-08 - скелет с верной моделью `c3500`, живой, с
    // включённой отрисовкой, и невидимый). Появлением пепел обязан анимации
    // материализации, а `-1` её и убирает. Строка блока рабочая, и всё, что в
    // ней уже стоит, вернее наших предположений.
    place_and_trigger(world, at, trigger);
    Some(BuddyHijack { rows, trigger, at: Instant::now() })
}

/// Поставить место призыва и дёрнуть триггер.
///
/// Место задать ОБЯЗАНЫ: смещения строки считаются от `spawn_origin`, а
/// заполняет его обычно потребление предмета. Без него призыв уезжает к началу
/// координат - живой, с моделью и невидимый (живьём 2026-09-08).
fn place_and_trigger(world: &mut WorldChrMan, at: HavokPosition, trigger: i32) {
    // Высоту отдаём как есть - ту, что нашёл луч на полу.
    //
    // Поправку тут пробовали трижды, и все три раза мимо: замер живьём
    // 2026-09-08 показал, что призыв не смещает существо на неё, а зеркалит
    // вниз. Пол 6.2, попросили 11.2, гость встал на 1.1 - ровно на поправку
    // НИЖЕ пола, и так во всех замерах. Значит верный ответ - ноль: тогда
    // просимая точка и есть пол, на который призыв сажает.
    world.summon_buddy_manager.spawn_origin = at;
    world.summon_buddy_manager.spawn_rotation = 0.0;
    free_summon_slot(world);
    world.summon_buddy_manager.request_summon_speffect_id = trigger;
}

/// С какой высоты существо считается крупным, метры.
///
/// Человекоподобные встают нормально и без поправки - это замер живьём
/// 2026-09-08, а не осторожность.
const BIG_CHR: f32 = 2.5;

/// Больше этого не поднимаем ни при каком размере, метры.
const LIFT_CAP: f32 = 6.0;

/// Поправка высоты для ОТЛАДОЧНОГО пути: он ставит существо ровно в заданную
/// точку, а у крупных точка привязки не в ногах, и они уходят в землю.
///
/// Призыва прахом это не касается - высоту он считает по-своему, см.
/// `place_and_trigger`.
fn lift_of(height: f32) -> f32 {
    if height <= BIG_CHR { 0.0 } else { (height * 0.5).min(LIFT_CAP) }
}

/// Высота существа по его строке парама.
fn height_of(repo: &SoloParamRepository, npc_param_id: i32) -> f32 {
    u32::try_from(npc_param_id)
        .ok()
        .and_then(|id| repo.get::<NpcParam>(id))
        .map_or(0.0, |r| r.hit_height())
}

/// Освободить слот призыва перед новым запросом.
///
/// **Пепел у игры один, и это не чинится.** Своим пеплом приходит только Слеза,
/// остальные - общим блоком, и держать их вместе движок не даёт: новый призыв
/// распускает прежний, кто бы в нём ни стоял. Проверено живьём 2026-09-08 -
/// живая масса стирала Слезу и наоборот, - и не зависит от того, обнуляем мы
/// здесь `active_summon_speffect_id` или нет: без обнуления результат тот же.
/// Возвращаться сюда незачем, пока Слеза ходит своим триггером.
///
/// Игра держит один пепел за раз и новый запрос при живом старом просто теряет
/// (живьём 2026-09-08: «тела нет - движок создал его и сразу потерял»). Уже
/// стоящих в мире это не трогает - мы обнуляем только учёт, и `tend_summon`
/// поднимает флаг обратно в ближайшие полсекунды.
fn free_summon_slot(world: &mut WorldChrMan) {
    let m = &mut world.summon_buddy_manager;
    m.player_has_alive_summon = false;
    m.active_summon_speffect_id = 0;
    m.disappear_requested = false;
    m.item_use_cooldown_timer = 0.0;
}

/// Блок строк `BuddyParam`, который занимаем под свой призыв.
///
/// `24000000` - Wandering Noble: **пять** строк на один триггер, максимум в
/// ванили. Пепел с несколькими строками призывает столько же существ разом, и
/// это единственный способ получить больше одного: активный пепел у игры один,
/// а дописать строку в рантайме нечем.
///
/// Лишние строки блока отключаются `npc_param_id = -1`. Отсюда и выбор от
/// одного до пяти.
///
/// **Блок Ложной Слезы (`20700000`) занимать нельзя** - от правки его полей
/// игра падает. Она призывается своим триггером и без единой правки, см.
/// `SpawnEntry::ash`.
const BUDDY_ROW: u32 = 24_000_000;

/// Сколько строк в этом блоке.
const BUDDY_ROWS: u32 = 5;

/// Анимация материализации двойника. У обычного существа она своя, у собранного
/// из внешности игрока её надо задать - иначе тело не появляется.
const MIMIC_ANIM: i32 = 60_500;

/// Всё, чем одна запись группы отличается от другой. Печатается и для нашего
/// призыва, и для настоящего пепла, призванного игроком: разница между ними и
/// есть ответ, которого из кода не видно.
fn summary(g: &SummonBuddyGroup) -> String {
    let head = format!(
        "bp {} dis {} delay {:.0} pt {} f {} warp {} doping {}",
        g.buddy_param_id,
        u8::from(g.disappear_requested),
        g.disappear_delay_sec,
        u8::from(g.has_spawn_point),
        g.follow_type,
        u8::from(g.warp_requested),
        g.doping_sp_effect_id,
    );
    // Существо читаем через указатель записи - иначе наше с настоящим прахом
    // не сравнить. Но только проверив его: игра убирает пепел раньше, чем
    // чистит группу, и разыменование висящей записи - это вылет.
    let raw = g.chr_ins.as_ptr() as usize;
    if !readable(raw) {
        return format!("{head} chr ?");
    }
    let chr = unsafe { g.chr_ins.as_ref() };
    format!(
        "{head} | team {} npc {} c{} type {:?} hp {} render {} dead {} grp {}",
        chr.team_type,
        chr.npc_param_id,
        chr.character_id,
        chr.chr_type,
        chr.modules.data.hp,
        u8::from(chr.chr_flags1c5.enable_render()),
        u8::from(chr.chr_flags1c5.death_flag()),
        u8::from(chr.chr_flags1c4.is_render_group_enabled()),
    )
}

/// Сколько секунд отодвигаем роспуск. Заведомо больше любого срока жизни.
const KEEP_ALIVE_DELAY: f32 = 9999.0;

/// Чем пепел праха ходит за игроком: `[Spirit Summon] Follow & Warp to Player`.
///
/// Это и есть весь механизм следования: игра вешает его призванному через
/// свободный слот эффектов `NpcParam`. Нам правки парама не нужно - эффект
/// ставится прямо на существо.
///
/// `297000` - следование и телепорт к игроку, весь механизм целиком.
/// `295000` - синее свечение призыва: зрителю сразу видно, что это союзник, а
/// не враг. Снимался на одну сборку по подозрению в невидимости - подозрение не
/// подтвердилось, гасило призыв обнуление полей строки пепла.
///
/// `296000` (профиль урона пепла) не берём: это правка баланса.
const SPIRIT_EFFECTS: [i32; 2] = [SPIRIT_FOLLOW, SPIRIT_GLOW];

/// Следование и телепорт к игроку - весь механизм целиком.
const SPIRIT_FOLLOW: i32 = 297_000;

/// Синее свечение призыва.
const SPIRIT_GLOW: i32 = 295_000;

/// Кому свечение не вешаем.
///
/// **Оно гасит существо целиком** (живьём 2026-09-09: Элден Бист союзником был
/// невидим, врагом виден). Похоже, свечение подменяет материал, а у существ со
/// своим шейдером подменять нечем - и на экране не остаётся ничего.
///
/// Список, а не поле строки: он на одного, а поле пришлось бы протаскивать
/// через три конструктора таблицы и две сотни строк.
const NO_GLOW: &[&str] = &["elden_beast"];

/// Как часто их подтверждаем. Эффект может истечь, а идти надо всё время.
const SPIRIT_EVERY: Duration = Duration::from_secs(2);

/// Как часто трогаем систему призыва. Каждый кадр она пересчитывает точку
/// появления и путь - живьём это стоило половины FPS.
const TEND_EVERY: Duration = Duration::from_millis(500);

/// Союзник прямо сейчас: чем он опознаётся, сколько здоровья и как далеко.
struct AllyNow {
    addr: usize,
    dist: f32,
}

/// Не дать системе призыва распустить наш пепел.
///
/// Все ручки разом, а не по одной: каждая проверка стоит запуска игры, а они
/// независимы. Что сработало, скажет диагностическая строка.
///
/// `ours` - хэндлы и здоровье наших союзников.
fn tend_summon(world: &mut WorldChrMan, stone: u32, write: bool, ours: &[AllyNow], doping: i32) -> String {
    let m = &mut world.summon_buddy_manager;
    // Читаем ДО своей записи: иначе строка показывала бы нашу же единицу.
    let zone = u8::from(m.is_within_activation_range);
    // Владельца у группы нет (`stone 0` живьём), а менеджер, не найдя своего
    // монумента, перестаёт считать призыв своим.
    if write && m.active_summmon_buddy_stone_entity_id == 0 {
        m.active_summmon_buddy_stone_entity_id = stone;
    }
    if write && m.buddy_stone_entity_id == 0 {
        m.buddy_stone_entity_id = stone;
    }
    if write {
        m.disappear_requested = false;
        // Только поднимаем, никогда не опускаем: у игрока может стоять свой
        // прах, и обнулить это значило бы распустить чужого.
        m.player_has_alive_summon = true;
    }

    // Группа - это то, чем менеджер владеет: из неё берётся следование
    // (по ней ходит `SummonBuddyWarpManager`), родной дисплей и срок жизни.
    //
    // Трогаем ТОЛЬКО свои записи, сверяя `chr_ins` с адресом нашего союзника.
    // Чужая группа - это чужой пепел, и лезть в неё нечем.
    //
    // И только то, что реально отличается: запись «как есть» каждый кадр
    // заставляла движок переигрывать появление, и звук призыва шёл очередью
    // (живьём 2026-09-08).
    let mut seen = 0;
    let mut mine = 0;
    let mut note = String::new();
    let mut reference = String::new();
    for pair in m.groups.iter_mut() {
        for g in pair.second.iter_mut() {
            seen += 1;
            let Some(a) = ours.iter().find(|a| g.chr_ins.as_ptr() as usize == a.addr) else {
                // Чужая запись - это НАСТОЯЩИЙ пепел, призванный игроком
                // руками. Читаем её как образец и не трогаем ничем: чем наш
                // призыв отличается от рабочего, из кода не выводится.
                if reference.is_empty() {
                    reference = summary(g);
                }
                continue;
            };
            mine += 1;
            if note.is_empty() {
                note = format!("{} d {:.0}", summary(g), a.dist);
            }
            // Ничего в записи не форсим, кроме срока роспуска: у настоящего
            // праха `has_spawn_point`, `follow_type` и `warp_requested` нулевые
            // (эталон 2026-09-08), следование даёт эффект на существе, а не
            // поля здесь. Срок отодвигаем только когда роспуск реально
            // запрошен - вне зоны монумента менеджер просит его каждый кадр.
            if write && g.disappear_requested && g.disappear_delay_sec < KEEP_ALIVE_DELAY {
                g.disappear_delay_sec = KEEP_ALIVE_DELAY;
            }
            // Усиление за уровень улучшения игра держит ЗДЕСЬ, номером эффекта,
            // и у нашего призыва оно пустое: пепел никто не улучшал. Ставим
            // свой - тот, чьи множители уже подогнаны под уровень игрока.
            // Прямое применение эффекта существу игра не приняла (живьём
            // 2026-09-08), а это её собственный путь.
            if write && doping > 0 && g.doping_sp_effect_id < 0 {
                g.doping_sp_effect_id = doping;
            }
        }
    }

    // Родной дисплей пепла игра нам не заполняет (`ash 0` живьём). Пробуем
    // сами: теперь призыв настоящий, и это не полоска из ниоткуда.
    let reference = if reference.is_empty() {
        String::new()
    } else {
        format!(" || REAL {reference}")
    };
    format!(" | zone {zone} | inGrp {seen} mine {mine} | MINE {note}{reference}")
}

/// Кто уже стоит в наборе пеплов праха. По разнице с этим списком и опознаём
/// свой призыв: `last_created_chr` система призыва не трогает.
///
/// Считаем только ЖИВЫХ. Набор держит слоты, а не существ, и пустой слот
/// попал бы в снимок - а призыв, занявший его же, своим бы не опознался.
fn buddy_handles(world: &WorldChrMan) -> Vec<FieldInsHandle> {
    world
        .summon_buddy_chr_set
        .characters()
        .filter(|c| c.modules.data.hp > 0)
        .map(|c| c.field_ins_handle)
        .collect()
}

/// Приходит ли этот гость своим пеплом, а не общим блоком.
///
/// Такому пересборка не нужна и вредна: строки его блока чужие, места в нашем
/// он не занимает, а снятие со сцены для него - просто потеря.
fn own_ash(key: &str) -> bool {
    entry(key).is_some_and(|e| e.ash != 0)
}

/// Строка пепла Ложной Слезы. Её поля править нельзя - от этого игра падает,
/// - но ЧИТАТЬ можно, и нужен оттуда ровно один номер: усиление за уровень.
const MIMIC_BUDDY_ROW: u32 = 20_700_000;

/// Насколько сильнее становится копия игрока: `(множитель урона, множитель
/// входящего урона)`.
///
/// Слеза - копия игрока и растёт вместе с ним: 3% урона и 2% сопротивления за
/// каждые полные десять уровней.
///
/// Проценты покрупнее пробовали (8 и 9) и вернулись к этим: при девяти
/// процентах сопротивление упирается в предохранитель уже к девяностому
/// уровню, и выше растёт только урон.
fn level_bonus(level: u32) -> (f32, f32) {
    let steps = (level / 10) as f32;
    // Нижний предел не для красоты: без него полсотни ступеней дали бы
    // неуязвимость, а дальше - отрицательный урон, то есть лечение ударами.
    (1.0 + 0.03 * steps, (1.0 - 0.02 * steps).max(0.2))
}

/// Сколько полей правит масштабирование: пять видов урона, восемь - защиты.
const ATTACK_RATES: usize = 5;
const RATES: usize = 13;

fn read_rates(r: &SP_EFFECT_PARAM_ST) -> [f32; RATES] {
    [
        r.physics_attack_power_rate(),
        r.magic_attack_power_rate(),
        r.fire_attack_power_rate(),
        r.thunder_attack_power_rate(),
        r.dark_attack_power_rate(),
        r.slash_damage_cut_rate(),
        r.blow_damage_cut_rate(),
        r.thrust_damage_cut_rate(),
        r.neutral_damage_cut_rate(),
        r.magic_damage_cut_rate(),
        r.fire_damage_cut_rate(),
        r.thunder_damage_cut_rate(),
        r.dark_damage_cut_rate(),
    ]
}

fn write_rates(r: &mut SP_EFFECT_PARAM_ST, v: [f32; RATES]) {
    r.set_physics_attack_power_rate(v[0]);
    r.set_magic_attack_power_rate(v[1]);
    r.set_fire_attack_power_rate(v[2]);
    r.set_thunder_attack_power_rate(v[3]);
    r.set_dark_attack_power_rate(v[4]);
    r.set_slash_damage_cut_rate(v[5]);
    r.set_blow_damage_cut_rate(v[6]);
    r.set_thrust_damage_cut_rate(v[7]);
    r.set_neutral_damage_cut_rate(v[8]);
    r.set_magic_damage_cut_rate(v[9]);
    r.set_fire_damage_cut_rate(v[10]);
    r.set_thunder_damage_cut_rate(v[11]);
    r.set_dark_damage_cut_rate(v[12]);
}

/// Всё, что мод меняет в строке усиления, - чтобы вернуть её как было.
#[derive(Clone, Copy)]
struct Doping {
    rates: [f32; RATES],
    /// Сколько эффект живёт и живёт ли по часам вообще.
    endurance: f32,
    life: bool,
    /// На кого он вообще ложится: сам, союзник, враг, ИИ, живой.
    target: [bool; 5],
}

fn read_doping(r: &SP_EFFECT_PARAM_ST) -> Doping {
    Doping {
        rates: read_rates(r),
        endurance: r.effect_endurance(),
        life: r.enable_life_time(),
        target: [
            r.effect_target_self(),
            r.effect_target_friend(),
            r.effect_target_enemy(),
            r.effect_target_ai(),
            r.effect_target_live(),
        ],
    }
}

fn write_doping(r: &mut SP_EFFECT_PARAM_ST, d: Doping) {
    write_rates(r, d.rates);
    r.set_effect_endurance(d.endurance);
    r.set_enable_life_time(d.life);
    r.set_effect_target_self(d.target[0]);
    r.set_effect_target_friend(d.target[1]);
    r.set_effect_target_enemy(d.target[2]);
    r.set_effect_target_ai(d.target[3]);
    r.set_effect_target_live(d.target[4]);
}

/// Подогнать усиление пепла под уровень игрока и вернуть, что переписали.
///
/// Своего множителя урона у существа нет вовсе - его считает движок из
/// speffect. Свой заводить неоткуда: строку в парам в рантайме не дописать.
/// Зато у пепла УЖЕ есть строка усиления за уровень улучшения
/// (`doping_sp_effect_lv0`) - её игра и вешает призванному. Правим числа в
/// ней, как правили обзор камеры: не результат, а то, из чего движок его
/// считает.
fn doping_ids(repo: &SoloParamRepository) -> Vec<u32> {
    let mut ids = Vec::new();
    // Только блок пепловой Слезы. Боссовому двойнику усиление не положено
    // (прямой запрос 2026-09-08): он и без того дерётся со всеми.
    for row in MIMIC_BUDDY_ROW..MIMIC_BUDDY_ROW + BUDDY_ROWS {
        let Some(r) = repo.get::<BuddyParam>(row) else { continue };
        // Одиннадцать ступеней улучшения, и какая достанется нашему призыву,
        // заранее не известно: живьём двойник пришёл не с той, что стоит в
        // первой строке блока. Берём все - они всё равно только его.
        for id in [
            r.doping_sp_effect_lv0(),
            r.doping_sp_effect_lv1(),
            r.doping_sp_effect_lv2(),
            r.doping_sp_effect_lv3(),
            r.doping_sp_effect_lv4(),
            r.doping_sp_effect_lv5(),
            r.doping_sp_effect_lv6(),
            r.doping_sp_effect_lv7(),
            r.doping_sp_effect_lv8(),
            r.doping_sp_effect_lv9(),
            r.doping_sp_effect_lv10(),
        ] {
            if let Ok(id) = u32::try_from(id) {
                if id > 0 && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

fn scale_mimic(repo: &mut SoloParamRepository, level: u32) -> Vec<(u32, Doping)> {
    let mut held = Vec::new();
    for id in doping_ids(repo) {
        let Some(row) = repo.get_mut::<SpEffectParam>(id) else { continue };
        let was = read_doping(row);
        let (atk, cut) = level_bonus(level);
        let mut now = was;
        for (i, v) in now.rates.iter_mut().enumerate() {
            *v *= if i < ATTACK_RATES { atk } else { cut };
        }
    // Строка писалась под улучшенный пепел, то есть под союзника и на время.
    // Наша Слеза бывает врагом и живёт до конца срока покупки, поэтому снимаем
    // и срок, и разборчивость в целях: иначе движок эффект просто не берёт
    // (живьём 2026-09-08 - поле группы проставлено, а на существе пусто).
        now.endurance = -1.0;
        now.life = false;
        now.target = [true; 5];
        write_doping(row, now);
        held.push((id, was));
    }
    held
}

/// Вернуть строку усиления как была.
fn restore_doping(repo: &mut SoloParamRepository, id: u32, was: Doping) {
    if let Some(row) = repo.get_mut::<SpEffectParam>(id) {
        write_doping(row, was);
    }
}

/// Есть ли пол вокруг точки, а не только под ней.
///
/// Четыре стороны света на `SPREAD_CHECK` - грубо, зато ровно про то, что нужно:
/// площадка, на которой пепел расставит своих, а не пятачок под одним из них.
fn room_around(phys: &CSPhysWorld, filter: u32, x: f32, y: f32, z: f32, owner: &PlayerIns) -> bool {
    [(SPREAD_CHECK, 0.0), (-SPREAD_CHECK, 0.0), (0.0, SPREAD_CHECK), (0.0, -SPREAD_CHECK)]
        .into_iter()
        .all(|(dx, dz)| ground_at(phys, filter, x + dx, y, z + dz, owner).is_some())
}

/// Начислить игроку руны за убитого гостя.
///
/// Игра за него не платит: призванный системой пеплов - дух, а за духа рун
/// нет. Раз мод сам его привёл, он сам и платит - читая ту же цену, что стоит
/// у существа в параме.
fn award_runes(world: &mut WorldChrMan, souls: u32) {
    if let Some(p) = world.main_player.as_mut() {
        let pgd = unsafe { p.player_game_data.as_mut() };
        pgd.rune_count = pgd.rune_count.saturating_add(souls);
    }
}

/// Убит, а не выгружен: существо на месте, но здоровья нет.
///
/// Разница важна: за убитого руны положены, за выгруженного переходом карты -
/// нет, игрок его и не трогал.
fn killed(world: &mut WorldChrMan, t: &TrackedSpawn) -> bool {
    chr_of(world, t).is_some_and(|c| c.modules.data.hp <= 0)
}

/// Что вернуть в одну строку `BuddyParam` после того, как призыв прочитан.
struct BuddyRowBackup {
    id: u32,
    npc: i32,
    think: i32,
    follow: u8,
    init: i32,
    anim: i32,
    /// Делит ли призванный цель с игроком.
    share: u8,
    /// Смещение строки от точки призыва - им игра расставляет пришедших веером.
    x: f32,
    z: f32,
}

/// Занятый блок строк целиком.
struct BuddyHijack {
    /// Пусто, если призывали чужим триггером и строк не трогали.
    rows: Vec<BuddyRowBackup>,
    /// Чем дёргали призыв.
    trigger: i32,
    at: Instant,
}

/// Сколько держим подменённую строку, прежде чем вернуть исходную.
///
/// Движок читает её на своём такте, не мгновенно. Меньше - призыв не успеет
/// прочитать, больше - настоящий пепел игрока всё это время призывал бы нашего.
const BUDDY_ROW_HOLD: Duration = Duration::from_secs(2);

/// Вернуть подменённые строки, если срок вышел.
fn restore_buddy_row(repo: &mut SoloParamRepository, hijack: &BuddyHijack, now: Instant) -> bool {
    if now.duration_since(hijack.at) < BUDDY_ROW_HOLD {
        return false;
    }
    for b in &hijack.rows {
        if let Some(row) = repo.get_mut::<BuddyParam>(b.id) {
            row.set_npc_param_id(b.npc);
            row.set_npc_think_param_id(b.think);
            row.set_pc_follow_type(b.follow);
            row.set_npc_player_init_param_id(b.init);
            row.set_generate_anim_id(b.anim);
            row.set_disable_pc_target_share(b.share);
            row.set_x_offset(b.x);
            row.set_z_offset(b.z);
        }
    }
    true
}

/// Пора ли переписывать команду. Отдельно от `write_team`, чтобы окно и
/// частоту можно было проверить без игры.
fn due_for_team_write(born: Instant, last: Option<Instant>, now: Instant) -> bool {
    now.duration_since(born) <= ALLY_REASSERT_FOR && due_every(last, now, ALLY_REASSERT)
}

/// Прошло ли `gap` с прошлого раза. `None` - ещё ни разу, значит пора.
fn due_every(last: Option<Instant>, now: Instant, gap: Duration) -> bool {
    !last.is_some_and(|at| now.duration_since(at) < gap)
}

/// Получился ли спавн: существо на месте, живое и с моделью.
fn took(world: &mut WorldChrMan, t: &TrackedSpawn) -> bool {
    chr_of(world, t).is_some_and(|chr| chr.modules.data.hp > 0 && has_model(chr))
}

/// Растормошить только что заспавненного.
///
/// **Вторая половина лечения «стоит и ничего не делает»** (жалоба 2026-08-20).
/// `activation_enabled` крейт описывает прямо: им управляет
/// `NPC_PARAM_ST::disableActivateOpen`. У боссов активация в открытом мире
/// выключена - их положено будить скриптом арены, которого при нашем спавне
/// нет и не будет. Отсюда и разница, которую видно живьём: солдат Годрика и
/// тролль-рыцарь нападают сразу, а Маления с Раданом стоят.
///
/// Заодно снимаем отладочные запреты - движок мог создать существо
/// «выключенным», и тогда не работает вообще ничего.
/// Убрать заспавненного. `force_unloaded` - штатный для движка способ увести
/// debug-персонажа в `Unloading`, а не убийство: труп остался бы лежать.
fn unload(world: &mut WorldChrMan, t: &TrackedSpawn) -> bool {
    match chr_of(world, t) {
        Some(chr) => {
            // **`debug_flags` в 2.7.0.0 трогать нельзя.** Запись туда
            // (`wake_up`) вешала игру намертво три раза подряд, а
            // `force_unloaded` из того же поля просто не действовал - ни по
            // TTL, ни по кнопке «Убрать всех» (живьём 2026-08-28). Оба факта
            // сходятся на одном: смещение поля уехало, и мы пишем биты не
            // туда.
            //
            // Поэтому существо не выгружаем, а убиваем: `hp` заведомо
            // читается верно (на нём стоит `alive`, и он отвечает правду).
            // Труп остаётся лежать - хуже, чем выгрузка, но лучше, чем враг,
            // которого нечем убрать.
            // Награду за него игрок не получает: он этого врага не убивал,
            // тот просто отжил своё. Игра держит рядом два флага «уже
            // выдано» - ими она и защищается от повторной выдачи, ими же
            // пользуемся и мы, ставя их ДО смерти.
            chr.chr_flags1c6.set_has_dropped_runes(true);
            chr.chr_flags1c6.set_had_dropped_item(true);
            chr.modules.data.hp = 0;
            chr.chr_flags1c5.set_death_flag(true);
            true
        }
        None => false,
    }
}

impl SpawnState {
    /// Что показывать в оверлее союзников. Пусто - союзников нет, и оверлей
    /// не рисуется вовсе.
    ///
    /// Отдаются только те, по кому тик успел снять показания: у свежего спавна
    /// в окне `SETTLE_GRACE` их ещё нет, и строка с нулевым здоровьем читалась
    /// бы как «уже труп».
    pub fn allies(&self) -> Vec<AllyView> {
        self.tracked.iter().filter_map(|t| t.view.clone()).collect()
    }

    pub fn active_count(&self) -> usize {
        self.tracked.len()
    }

    /// Сколько таких уже есть или вот-вот будет: в мире, в отложенных и в
    /// заявке. Заявка считается наравне - иначе две быстрые покупки
    /// проскочили бы лимит, пока движок не подтвердил первую.
    fn same_count(&self, key: &str) -> usize {
        self.tracked.iter().filter(|t| t.key == key).count()
            + self.delayed.iter().filter(|d| d.key == key).count()
            + usize::from(self.awaiting.as_ref().is_some_and(|a| a.key == key))
    }

    /// Ключ конкретного врага. Обычный проходит как есть, псевдо-ключ
    /// «случайный» превращается в настоящий прямо сейчас - в момент покупки,
    /// а не при настройке награды.
    ///
    /// **Лотерея повторов не даёт**: кандидат, который уже стоит в мире, из
    /// розыгрыша выпадает вовсе. Явный выбор врага этим не ограничен - хочет
    /// стример троих импов, будут трое.
    fn roll(&mut self, key: &'static str) -> Result<&'static str, SpawnRejected> {
        let Some(pick) = random_pick(key) else {
            return entry(key).map(|e| e.key).ok_or(SpawnRejected::UnknownEnemy);
        };
        let blamed = blamed();
        let free: Vec<&'static str> = SPAWN_TABLE
            .iter()
            .filter(|e| pick.tier.is_none_or(|t| e.tier == t))
            // Уронивший игру в прошлый запуск в лотерею не идёт. Явный выбор
            // стримера это не трогает: там он решает сам.
            .filter(|e| !blamed.contains(&e.key))
            .filter(|e| free_for_roll(self.same_count(e.key)))
            .map(|e| e.key)
            .collect();
        if free.is_empty() {
            return Err(SpawnRejected::SameLimitReached);
        }
        Ok(free[xorshift32(&mut self.rng) as usize % free.len()])
    }

    /// Забрать сообщение о последней заявке. Забирается, а не читается: одно
    /// событие - одно сообщение, иначе прошлое висело бы в окне вечно.
    /// Снять со сцены всех, кто пришёл пеплом, и вернуть их описания.
    ///
    /// Пепел пересобирается целиком на каждую покупку, поэтому прежних сначала
    /// убираем, а потом призываем заново - тех же и с тем же здоровьем.
    /// Догоревшие по сроку не переносятся.
    fn take_summoned(&mut self, world: &mut WorldChrMan, now: Instant) -> Vec<AshMember> {
        let mut out = Vec::new();
        let mut keep = Vec::new();
        for mut t in std::mem::take(&mut self.tracked) {
            // Пришедшего своим пеплом не трогаем: он в блоке не стоит, и
            // пересборка его только уничтожила бы (живьём 2026-09-08 - призыв
            // Слезы стирал уже стоящих, и наоборот).
            if !t.summoned || own_ash(t.key) {
                keep.push(t);
                continue;
            }
            // Место снимаем вместе со здоровьем: пепел призывается заново на
            // каждую покупку из ОДНОЙ точки, и без этого прежние сбегались бы
            // к новому гостю.
            let (hp, at) = chr_of(world, &t)
                .map_or((0, t.anchor), |c| (c.modules.data.hp.max(0), c.modules.physics.position));
            unload(world, &t);
            let ttl = t.expires_at.saturating_duration_since(now);
            if hp <= 0 || ttl.is_zero() {
                continue;
            }
            out.push(AshMember {
                at,
                key: t.key,
                team: t.team,
                npc: t.npc,
                think: t.think,
                init: entry(t.key).map_or(0, |e| e.init),
                ally: t.ally,
                hp,
                ttl,
                viewer: std::mem::take(&mut t.viewer),
                reward_id: std::mem::take(&mut t.reward_id),
                redemption_id: std::mem::take(&mut t.redemption_id),
                confirmed: t.confirmed,
            });
        }
        self.tracked = keep;
        out
    }

    pub fn take_note(&mut self) -> Option<String> {
        self.note.take()
    }

    /// Строки для отладочного окна (`debug = true`).
    ///
    /// Формат тот же, что разбирает `overlay::draw_debug`: `# ` - заголовок,
    /// два пробела - строка под ним.
    ///
    /// Здесь живёт ТОЛЬКО то, что сейчас выясняется. Подтверждённое уезжает
    /// отсюда сразу, иначе окно превращается в ленту, в которой не видно
    /// текущего вопроса.
    pub fn probe(&self) -> Vec<String> {
        let mut out = vec!["# СПАВН".to_string()];
        if let Some(v) = self.verdict.as_deref() {
            out.push(format!("  {v}"));
        }
        if !self.place_note.is_empty() {
            out.push(format!("  {}", self.place_note));
        }
        out.extend(self.debug.iter().cloned());
        out
    }

    /// Бросить заявку в полёте, не дожидаясь результата. Нужно на переходах,
    /// после которых `tick` больше не будет вызываться (ушли из игры,
    /// аварийный выключатель): баллы за неё надо вернуть.
    ///
    /// Уже стоящих в мире (`tracked`) не трогаем. Их TTL идёт по стенным
    /// часам и вне геймплея НЕ замирает - вернувшись из долгого меню, игрок
    /// увидит, что просроченные снимаются первым же геймплейным кадром.
    pub fn abandon_all(&mut self) -> Vec<(String, String)> {
        blow_out_candle();
        let mut out: Vec<(String, String)> =
            self.delayed.drain(..).map(|d| (d.reward_id, d.redemption_id)).collect();
        out.extend(self.awaiting.take().map(|a| (a.reward_id, a.redemption_id)));
        out
    }

    /// Погашения, которые ещё не довели до врага в мире: отложенные и та, что
    /// в полёте. См. `actions::pending_ids` - у Twitch они тоже «не выполнено».
    pub fn pending_ids(&self) -> Vec<String> {
        self.delayed
            .iter()
            .map(|d| d.redemption_id.clone())
            .chain(self.awaiting.iter().map(|a| a.redemption_id.clone()))
            .collect()
    }

    /// Бросить отложенные покупки, не трогая уже стоящих в мире врагов.
    ///
    /// Игрок умер - враг, дозревающий к появлению, вылезет над трупом или у
    /// благодати, то есть не там и не тогда, за что платили.
    pub fn abandon_delayed(&mut self) -> Vec<(String, String)> {
        self.delayed.drain(..).map(|d| (d.reward_id, d.redemption_id)).collect()
    }

    /// Есть ли чем заняться: заявка в полёте, отложенные покупки или живые
    /// враги с TTL. Пока `false` - синглтон не резолвим вовсе.
    pub fn has_work(&self) -> bool {
        !self.tracked.is_empty()
            || !self.delayed.is_empty()
            || self.awaiting.is_some()
            // Подменённую строку пепла надо вернуть на место, даже если ни
            // одного спавна не осталось: иначе собственный пепел игрока до
            // конца сессии призывал бы нашего.
            || self.buddy_row.is_some()
            // Одолженный `is_buddy_ai` тоже надо вернуть.
            || self.ai_hold.is_some()
    }

    /// Снять всех заспавненных немедленно - аварийная кнопка в настройках.
    /// Возвращает, сколько записей сняли.
    pub fn clear_all(&mut self) -> usize {
        let Ok(world) = (unsafe { WorldChrMan::instance_mut() }) else {
            // Мира нет - существ тоже, запись держать незачем.
            return std::mem::take(&mut self.tracked).len();
        };
        let mut n = 0;
        for t in std::mem::take(&mut self.tracked) {
            if unload(world, &t) {
                n += 1;
            }
        }
        n
    }

    /// Ник покупателя этого врага, если он заспавнен за баллы.
    ///
    /// Пустой ник (тестовая кнопка) не в счёт: подписывать врага пустотой
    /// незачем, пусть достанется обычной ротации.
    pub fn owner_of(&self, handle: &FieldInsHandle) -> Option<&str> {
        self.owners().find(|(h, _)| *h == handle).map(|(_, name)| name)
    }

    /// Пары «хэндл - ник покупателя»: живые спавны и те, что только что
    /// умерли, но чью полоску игра ещё рисует.
    fn owners(&self) -> impl Iterator<Item = (&FieldInsHandle, &str)> {
        self.tracked
            .iter()
            .filter(|t| !t.viewer.is_empty())
            .map(|t| (&t.handle, t.viewer.as_str()))
            .chain(
                self.ghosts
                    .iter()
                    .filter(|(_, _, at)| at.elapsed() < GHOST_LINGER)
                    .map(|(h, name, _)| (h, name.as_str())),
            )
    }

    /// Есть ли хоть один заспавненный с ником. Нужно, чтобы подписи рисовались
    /// даже когда чат не подключён: покупатель известен и без списка зрителей.
    pub fn has_owners(&self) -> bool {
        self.owners().next().is_some()
    }

    /// Хэндлы купленных врагов - их подписываем в обход общих правил отбора
    /// (см. `enemies::native_tags`).
    pub fn owned_handles(&self) -> Vec<FieldInsHandle> {
        self.owners().map(|(h, _)| *h).collect()
    }

    /// Есть ли прямо сейчас в мире хоть один наш враг - для счётчика «Смерть
    /// от зрителей».
    ///
    /// Вопрос намеренно грубый: «кто именно добил» игра надёжно не отвечает.
    /// У выстрела, заклинания и любой AoE в `last_hit_by` стоит САМ СНАРЯД, а
    /// не тот, кто его пустил (та же особенность, что разбиралась в руническом
    /// обмене), - и первая версия счётчика, стоявшая на этом поле, живьём не
    /// сработала ни разу (жалоба 2026-08-25). Босс тем более дерётся не одними
    /// кулаками.
    ///
    /// `tracked` держит только живых - мёртвых `tick` забывает в тот же кадр,
    /// - а `ghosts` закрывают случай «добил и сам исчез».
    ///
    /// Известный потолок: убежал от гостя и умер от чего-то своего - зачтётся
    /// зрителю. Пока гость жив, это чаще правда, чем нет.
    pub fn any_alive(&self) -> bool {
        !self.tracked.is_empty()
            || self.ghosts.iter().any(|(_, _, at)| at.elapsed() < GHOST_LINGER)
    }

    /// Запомнить ник покупателя за врагом, которого больше нет среди живых.
    fn remember_ghost(&mut self, handle: FieldInsHandle, viewer: String) {
        if viewer.is_empty() {
            return;
        }
        // Восемь записей - потолок с запасом: дольше `GHOST_LINGER` тут никто
        // не живёт, а чистить их регулярно негде (`tick` не зовётся, когда
        // спавнов не осталось вовсе).
        if self.ghosts.len() >= 8 {
            self.ghosts.remove(0);
        }
        self.ghosts.push((handle, viewer, Instant::now()));
    }

    /// Заспавнить врага по ключу куратор-списка. Синглтон резолвится здесь, а
    /// не в `lib.rs`: игровую память трогают только `stats.rs`, `enemies.rs` и
    /// этот модуль, наружу отсюда выходят плоские данные.
    ///
    /// Спавн - редкое событие, поэтому дорогой резолв через рефлексию
    /// Dantelion2 тут допустим (в отличие от `collect`, где он ронял FPS).
    /// Поставить покупку в очередь на отложенный спавн. Проверки, которые
    /// можно сделать сразу, делаются сразу: возвращать баллы через минуту
    /// ожидания - худший вариант из всех.
    #[allow(clippy::too_many_arguments)]
    pub fn schedule(
        &mut self,
        key: &'static str,
        delay: Duration,
        ttl: Duration,
        limit: u32,
        reward_id: &str,
        redemption_id: &str,
        viewer: &str,
        ally: bool,
    ) -> Result<(), SpawnRejected> {
        let key = self.roll(key)?;
        // Отложенные занимают слоты наравне со стоящими в мире: иначе за время
        // ожидания можно накупить сверх лимита, и он сработал бы вхолостую.
        if !slot_available(self.tracked.len() + self.delayed.len(), self.awaiting.is_some(), limit) {
            return Err(SpawnRejected::LimitReached);
        }
        self.delayed.push(DelayedSpawn {
            key,
            due: Instant::now() + delay,
            ttl,
            ally,
            viewer: viewer.to_string(),
            reward_id: reward_id.to_string(),
            redemption_id: redemption_id.to_string(),
        });
        Ok(())
    }

    /// Возвращает `(на возврат баллов, на пометку «выполнено»)`.
    ///
    /// Второе - это те покупки, чей враг реально появился в мире. Раньше срока
    /// их метить нельзя: пока заявка в полёте, она ещё может протухнуть, а
    /// помеченное выполненным Twitch отменять уже не даёт.
    #[allow(clippy::too_many_arguments)]
    pub fn tick_world(
        &mut self,
        radius_m: f32,
        debug_spawn: bool,
        in_front: bool,
        limit: u32,
        now: Instant,
    ) -> (Vec<(String, String)>, Vec<(String, String)>) {
        let mut refunds = Vec::new();
        let mut done = Vec::new();
        let Ok(world) = (unsafe { WorldChrMan::instance_mut() }) else {
            return (refunds, done);
        };
        // Одна созревшая заявка за кадр: у движка всё равно один флаг спавна,
        // и вторая в тот же кадр всё равно получила бы `Busy`.
        if self.awaiting.is_none() {
            if let Some(i) = self.delayed.iter().position(|d| now >= d.due) {
                let d = self.delayed.remove(i);
                let outcome = match (entry(d.key), player_position(world)) {
                    (Some(e), Some(pos)) => self.request(
                        world,
                        e,
                        pos,
                        radius_m,
                        debug_spawn,
                        in_front,
                        limit,
                        &d.reward_id,
                        &d.redemption_id,
                        &d.viewer,
                        d.ttl,
                        d.ally,
                    ),
                    _ => Err(SpawnRejected::NoPlayer),
                };
                match outcome {
                    Ok(()) => {}
                    // `Busy` - состояние на один кадр (движок ещё не разобрал
                    // чужую заявку), а не отказ. Возвращать за него баллы
                    // значило бы отбить покупку по своей же вине - кладём
                    // обратно в очередь и пробуем на следующем кадре.
                    Err(SpawnRejected::Busy) => self.delayed.insert(i, d),
                    Err(_) => refunds.push((d.reward_id, d.redemption_id)),
                }
            }
        }
        let (settled, fulfilled) = self.tick(world, now);
        refunds.extend(settled);
        done.extend(fulfilled);
        (refunds, done)
    }

    /// Запросить спавн. Проверки - лимит, затем существование npc_param_id в
    /// парамах (точечный лукап, не полный проход, чтобы не резолвить
    /// `SoloParamRepository` на каждый кадр). Сам вызов в память игры -
    /// асинхронный: результат смотрим в `tick` на следующих кадрах.
    #[allow(clippy::too_many_arguments)]
    fn request(
        &mut self,
        world: &mut WorldChrMan,
        entry: &SpawnEntry,
        player_pos: HavokPosition,
        radius_m: f32,
        debug_spawn: bool,
        in_front: bool,
        limit: u32,
        reward_id: &str,
        redemption_id: &str,
        viewer: &str,
        ttl: Duration,
        ally: bool,
    ) -> Result<(), SpawnRejected> {
        if !slot_available(self.tracked.len(), self.awaiting.is_some(), limit) {
            return Err(SpawnRejected::LimitReached);
        }
        // Смещение поля могло уехать после патча игры - тогда любое касание
        // `debug_chr_creator` пишет в чужую память и роняет игру.
        if !creator_ok(world) {
            return Err(SpawnRejected::EngineMoved);
        }
        // Заявка одна на всех: у движка один флаг `spawn` и одна `init_data`.
        // Второй вызов до того, как он их подхватил, затирает параметры первого
        // - первый враг либо не появится вовсе, либо появится чужим, а наша
        // запись о нём потеряется вместе с TTL и возвратом баллов.
        if self.awaiting.is_some() || world.debug_chr_creator.spawn {
            return Err(SpawnRejected::Busy);
        }
        // Строка парама обязана существовать: спавн по несуществующей - краш.
        // Заодно это признак «парамы уже загружены».
        let repo = unsafe { SoloParamRepository::instance() }.map_err(|_| SpawnRejected::UnknownNpcParam)?;
        // У призываемого своим пеплом строки парама нет - его собирает игра,
        // и команду ему ставим по общему правилу.
        let param_team = if entry.ash != 0 {
            Some(0)
        } else {
            u32::try_from(entry.npc_param_id)
                .ok()
                .and_then(|id| repo.get::<NpcParam>(id))
                .map(|r| r.team_type())
        };
        let Some(param_team) = param_team else {
            return Err(SpawnRejected::UnknownNpcParam);
        };
        let height = height_of(repo, entry.npc_param_id);
        let team = if ally {
            TEAM_ALLY
        } else {
            match param_team {
                0 => TEAM_ENEMY,
                t => t,
            }
        };
        // Свеча зажигается ДО обхода мира и лучей, а не перед самим вызовом
        // движка: упасть можно на любом из этих этапов, а ранних выходов
        // дальше уже нет.
        self.verdict = None;
        light_candle(entry.key, "think");
        // Призываемому своим пеплом ИИ назначает игра. Обход мира ради него -
        // это проход по всем наборам существ впустую.
        let think = match (entry.ash, entry.think) {
            (0, 0) => think_param_id(world, repo, entry.npc_param_id),
            // Призываемому своим пеплом ИИ назначает игра, а у двойника номер
            // задан явно: усечением его не найти, он из чужого диапазона.
            _ => entry.think,
        };

        light_candle(entry.key, "rays");
        let at = pick_position(world, player_pos, radius_m, in_front, &mut self.rng);
        let request = ChrDebugSpawnRequest {
            chr_id: entry.chr_id(),
            // У обычного врага параметров отыгрыша нет (`-1`); они есть только
            // у двойников, слепленных из модели игрока.
            // Параметры отыгрыша персонажа (игрок, фантомы). Всегда `-1`:
            // спавн существ НА МОДЕЛИ ИГРОКА через этот путь не работает
            // вовсе: движок создаёт тело без модели (проверено живьём на
            // ванильных числах Ложной Слезы и Серебряной слезы).
            chara_init_param_id: -1,
            npc_param_id: entry.npc_param_id,
            npc_think_param_id: think,
            event_entity_id: -1,
            talk_id: -1,
            pos_x: at.0,
            // Точка от лучей - это высота ПОЛА, а не место, где у модели точка
            // привязки. У крупных она бывает в середине туши, и существо
            // оказывается по пояс в земле (живьём 2026-09-08: Астель вставал
            // нормально, Плацидусакс - наполовину). Поправка та же, что у
            // праха, и от той же высоты в параме.
            pos_y: at.1 + lift_of(height),
            pos_z: at.2,
        };
        let previous_last_created =
            world.debug_chr_creator.last_created_chr.map_or(0, |p| p.as_ptr() as usize);
        light_candle(entry.key, "spawn");
        // Призыв идёт СИСТЕМОЙ ПЕПЛОВ ПРАХА, а не отладочным создателем: она
        // собирает существо целиком, а отладочный ассеты не грузит и оставляет
        // невидимые тела. Союзнику она вдобавок даёт следование.
        //
        // Активный пепел у игры ОДИН, и новый запрос убирает прошлый. Поэтому
        // каждая покупка ПЕРЕСОБИРАЕТ его целиком: прежние призванные плюс
        // новый. Здоровье и место переносятся - избитый враг не должен
        // вернуться целым, а прежние не должны сбежаться к игроку.
        //
        // Больше пяти в пепел не помещается: столько строк в самом широком
        // блоке, а дописать строку в рантайме нечем. Поэтому пепел набирается
        // до потолка из настроек, а всё сверх идёт отладочным создателем -
        // общий лимит в мире так ничем не ограничен, кроме `spawn_limit`.
        //
        // Ложная Слеза не делит пепел ни с кем: строки её блока трогать нельзя,
        // и приходит она своим триггером в одиночку.
        let now = Instant::now();
        // Пепел всегда набирается под потолок блока: настройки на это не было
        // и не нужно - меньше держать незачем, а больше игра не умеет.
        let cap = BUDDY_ROWS as usize;
        // Своим пеплом и на модели игрока - только через призыв: отладочный
        // создатель ни того, ни другого не собирает. Такому пепел уступают
        // всегда, даже когда он полон.
        let only_ash = entry.ash != 0 || entry.init != 0;
        // Пепел - основной путь, а галочка отладочного спавна - обход его
        // потолка: тот не грузит ассеты, оставляет невидимые тела и не даёт
        // следования, зато пускает в мир сколько угодно.
        //
        // Союзника он не касается никогда: следование - это и есть весь смысл
        // союзника, а даёт его только пепел (прямой запрос 2026-09-08).
        //
        // Крупные раньше уходили отладочному создателю в обход галочки: прах
        // ставит гостей сам, и большие у него тонули. Теперь место назначает
        // мод (`keep_placed`), а не прах, и делить существ по росту незачем.
        let use_ash = only_ash || ally || !debug_spawn;
        // Заявка целиком - одной строкой в отладочное окно: «наполовину в
        // земле» ничего не говорит, пока не видно, какую высоту мод прочитал у
        // существа и куда просил его поставить.
        self.place_note = format!(
            "{} h {height:.1} lift {:.1} | ask {:.1}",
            if use_ash { "ash" } else { "debug" },
            lift_of(height),
            at.1 + lift_of(height),
        );
        // Свой пепел общий блок не пересобирает: прежние гости остаются, где
        // стояли, а этот приходит рядом со своим триггером.
        let mut members =
            if use_ash && entry.ash == 0 { self.take_summoned(world, now) } else { Vec::new() };
        // Приходящий своим пеплом в блок не помещается - он там и не жил.
        members.retain(|m| m.npc != 0);
        members.push(AshMember {
            at,
            key: entry.key,
            team,
            npc: entry.npc_param_id,
            think,
            init: entry.init,
            ally,
            hp: 0,
            ttl,
            viewer: viewer.to_string(),
            reward_id: reward_id.to_string(),
            redemption_id: redemption_id.to_string(),
            confirmed: false,
        });
        // В блок больше пяти не влезает, и лишний просто не появился бы: он уже
        // снят со сцены, а строки для него нет. Уходят самые старые.
        if members.len() > cap {
            members.drain(..members.len() - cap);
        }
        let (previous_buddies, buddy_row) = if use_ash {
            // Снимок берём ДО того, как дёрнем триггер: успей движок отработать
            // в тот же кадр, новичок попал бы в «уже были» и своим не опознался.
            let before = buddy_handles(world);
            let repo = unsafe { SoloParamRepository::instance_mut() }
                .map_err(|_| SpawnRejected::UnknownNpcParam)?;
            if self.stone == 0 {
                self.stone = repo
                    .rows::<BuddyStoneParam>()
                    .map(|(_, r)| r.talk_chr_entity_id())
                    .find(|id| *id != 0)
                    .unwrap_or(0);
            }
            self.ash_think = think;
            self.buddy_ai = 9;
            // Буддийский ИИ - каждому союзнику пепла, а не только тому, кого
            // купили сейчас: пепел пересобирается целиком, и прежний союзник
            // иначе возвращался бы с обычным ИИ.
            //
            // Врагу его не даём: он означал бы, что тот ходит за игроком по
            // пятам. Известный потолок: союзник и враг одного вида делят одну
            // строку поведения, и тогда побеждает союзник - развести их нечем.
            if self.ai_hold.is_none() {
                let mut held: Vec<(u32, u8)> = Vec::new();
                for m in members.iter().filter(|m| m.ally) {
                    let Ok(row) = u32::try_from(m.think) else { continue };
                    if held.iter().any(|(id, _)| *id == row) {
                        continue;
                    }
                    if let Some(r) = repo.get_mut::<NpcThinkParam>(row) {
                        held.push((row, r.is_buddy_ai()));
                        r.set_is_buddy_ai(1);
                        self.buddy_ai = 1;
                    }
                }
                if !held.is_empty() {
                    self.ai_hold = Some((Instant::now(), held));
                }
            }
            match summon_via_ash(world, repo, entry.ash, &members, at) {
                Some(hijack) => (before, Some(hijack)),
                // Строк пепла не нашлось - это либо парамы ещё не загружены,
                // либо занятый блок кто-то переписал. Прежний создатель тут
                // запасной, и только по галочке.
                None if only_ash || !debug_spawn => {
                    return Err(SpawnRejected::UnknownNpcParam)
                }
                None => {
                    world.spawn_debug_character(&request);
                    members.drain(..members.len() - 1);
                    (Vec::new(), None)
                }
            }
        } else {
            world.spawn_debug_character(&request);
            // Пепел не трогали: в заявке только новый.
            members.drain(..members.len() - 1);
            (Vec::new(), None)
        };
        let buddy_trigger = buddy_row.as_ref().map_or(0, |b| b.trigger);
        if buddy_row.is_some() {
            self.buddy_row = buddy_row;
        }
        if buddy_trigger != 0 {
            // Латиница, а не русский: в атлас печётся только текст активной
            // локали, а это строка отладочного окна.
            let m = &world.summon_buddy_manager;
            self.place_note.push_str(&format!(
                " || row {BUDDY_ROW} spe {buddy_trigger} | buddies {} | zone {} | has {} | active {} | cd {:.1}",
                previous_buddies.len(),
                u8::from(m.is_within_activation_range),
                u8::from(m.player_has_alive_summon),
                m.active_summon_speffect_id,
                m.item_use_cooldown_timer,
            ));
        }
        self.awaiting = Some(AwaitingSpawn {
            key: entry.key,
            reward_id: reward_id.to_string(),
            redemption_id: redemption_id.to_string(),
            previous_last_created,
            previous_buddies,
            buddy_trigger,
            members,
            deadline: Instant::now()
                + if ally { ALLY_AWAIT_TIMEOUT } else { AWAIT_TIMEOUT },
        });
        Ok(())
    }

    /// Каждый геймплейный кадр, пока есть что делать (см. точку вызова в
    /// `lib.rs` - не резолвим `instance_mut` впустую, когда спавнов нет
    /// вовсе). Снимает результат заявки, ставит TTL, чистит просроченное.
    /// Возвращает `(reward_id, redemption_id)` на возврат баллов, если заявка
    /// протухла, не дождавшись движка.
    fn tick(
        &mut self,
        world: &mut WorldChrMan,
        now: Instant,
    ) -> (Vec<(String, String)>, Vec<(String, String)>) {
        let mut refunds = Vec::new();
        let mut fulfilled = Vec::new();
        if self.awaiting.is_some() && !creator_ok(world) {
            // Проверять и здесь: между заявкой и её разбором ничего не
            // меняется, но читать мусорный указатель нельзя и один раз.
            refunds.extend(self.awaiting.take().map(|a| (a.reward_id, a.redemption_id)));
        }
        // Подменённую строку `BuddyParam` возвращаем на место, как только
        // движок её прочитал: иначе настоящие пеплы игрока до конца сессии
        // призывали бы нашего.
        if let Some(backup) = self.buddy_row.as_ref() {
            if let Ok(repo) = unsafe { SoloParamRepository::instance_mut() } {
                if restore_buddy_row(repo, backup, now) {
                    self.buddy_row = None;
                }
            }
        }
        // Показания для отладочного окна - два раза в секунду, не каждый кадр:
        // по одному кадру не видно ни того, забрал ли движок запрос, ни того,
        // что стало с призванным дальше. Здесь же идут и сами записи в систему
        // призыва (`tend_summon`), поэтому обход не лишний.
        let write = due_every(self.last_tend, now, TEND_EVERY);
        let watching = write
            && (self.awaiting.as_ref().is_some_and(|a| a.buddy_trigger != 0)
                || self.tracked.iter().any(|t| t.summoned));
        if watching {
            self.last_tend = Some(now);
            let at = player_position(world);
            let ours: Vec<AllyNow> = self
                .tracked
                .iter()
                .filter(|t| t.summoned)
                .filter_map(|t| {
                    let chr = world.chr_ins_by_handle(&t.handle)?;
                    if chr.modules.data.hp <= 0 {
                        return None;
                    }
                    let p = chr.modules.physics.position;
                    let dist = at.map_or(0.0, |q| {
                        ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2) + (p.2 - q.2).powi(2)).sqrt()
                    });
                    Some(AllyNow { addr: t.addr, dist })
                })
                .collect();
            // Возраст самого старого союзника: по нему видно, доживают ли
            // призванные до своего срока.
            let age = self
                .tracked
                .iter()
                .filter(|t| t.ally)
                .map(|t| now.duration_since(t.born).as_secs_f32())
                .fold(0.0_f32, f32::max);
            let tended = if ours.is_empty() {
                String::new()
            } else {
                tend_summon(
                    world,
                    self.stone,
                    write,
                    &ours,
                    self.doping_hold.first().map_or(-1, |(id, _)| id.cast_signed()),
                )
            };
            self.debug = vec![
                format!(
                    "  alive {} of {} | ai {} buddyAI {} | age {age:.0}",
                    ours.len(),
                    buddy_handles(world).len(),
                    self.ash_think,
                    self.buddy_ai,
                ),
                format!("  {tended}"),
            ];
            // Строка на гостя: где он и стоит ли. По ней и видно, попала ли
            // расстановка.
            for t in self.tracked.iter().filter(|t| t.summoned) {
                let Some(chr) = world.chr_ins_by_handle(&t.handle) else { continue };
                let p = chr.modules.physics.position;
                self.debug.push(format!(
                    "  {} {} y {:.1} want {:.1} stand {} placed {}",
                    t.key,
                    if t.ally { "ally" } else { "foe" },
                    p.1,
                    t.anchor.1,
                    u8::from(chr.modules.physics.standing_on_solid_ground),
                    u8::from(t.placed),
                ));
            }
        }
        if let Some(a) = &self.awaiting {
            // Пока флаг `spawn` стоит, движок заявку ещё не разбирал, и
            // `last_created_chr` - от прошлого раза. Принимать его за своего
            // нельзя, даже если адрес отличается.
            let handled = !world.debug_chr_creator.spawn;
            // Система призыва отдаёт СКОЛЬКО УГОДНО существ за один запрос:
            // сколько строк блока занято, столько и придёт. Забираем всех -
            // иначе четверо из пяти остались бы без срока жизни и без учёта в
            // лимите, то есть в мире навсегда.
            let fresh: Vec<(FieldInsHandle, usize, i32)> = if a.buddy_trigger == 0 {
                world
                    .debug_chr_creator
                    .last_created_chr
                    .filter(|_| handled)
                    .filter(|p| p.as_ptr() as usize != a.previous_last_created)
                    .map(|p| {
                        let c = unsafe { p.as_ref() };
                        (c.field_ins_handle, p.as_ptr() as usize, c.npc_param_id)
                    })
                    .into_iter()
                    .collect()
            } else {
                world
                    .summon_buddy_chr_set
                    .characters()
                    .filter(|c| {
                        c.modules.data.hp > 0 && !a.previous_buddies.contains(&c.field_ins_handle)
                    })
                    .map(|c| (c.field_ins_handle, std::ptr::from_mut(c) as usize, c.npc_param_id))
                    .collect()
            };
            if !fresh.is_empty() {
                let summoned = a.buddy_trigger != 0;
                // Команду и эффекты откладываем до приговора: пока движок
                // собирает существо, любое наше касание в этом окне трижды
                // подряд вешало игру живьём.
                //
                // Покупку НЕ подтверждаем здесь: движок только что отдал
                // указатель, а появилось ли по нему что-то видимое, станет
                // известно через `SETTLE_GRACE`.
                let taken = self.awaiting.take();
                let mut members = taken.map(|a| a.members).unwrap_or_default();
                // Движок заявку принял: дальше он собирает модель, и упасть
                // может уже на ней - свеча горит до приговора, только этапом
                // ниже.
                light_candle(members.last().map_or("", |m| m.key), "settle");
                // Существ может прийти больше, чем мы просили: у Ложной Слезы
                // в блоке две строки - сгусток и сам двойник. Лишнего всё равно
                // надо взять на учёт, иначе он останется в мире навсегда,
                // дружественным и без срока жизни (живьём 2026-09-08: Слеза
                // приходила союзной, а повторная покупка роняла FPS).
                let spare = members.last().cloned();
                for (handle, addr, npc) in fresh {
                    // Кто есть кто, решает строка парама: порядок, в котором
                    // движок кладёт призванных в набор, нам не обещан.
                    let i = members
                        .iter()
                        .position(|m| m.npc == npc)
                        .or(if members.is_empty() { None } else { Some(0) });
                    let m = match i {
                        Some(i) => members.remove(i),
                        // Лишний идёт без оплаты: платили один раз.
                        None => match spare.clone() {
                            Some(m) => AshMember {
                                reward_id: String::new(),
                                redemption_id: String::new(),
                                confirmed: true,
                                hp: 0,
                                ..m
                            },
                            None => continue,
                        },
                    };
                    self.tracked.push(TrackedSpawn {
                        souls: 0,
                        anchor: m.at,
                        placed: false,
                        key: m.key,
                        team: m.team,
                        ally: m.ally,
                        summoned,
                        view: None,
                        lag_from: 0.0,
                        lag_at: now,
                        last_team_write: None,
                        last_spirit: None,
                        reward_id: m.reward_id,
                        redemption_id: m.redemption_id,
                        confirmed: m.confirmed,
                        handle,
                        addr,
                        born: now,
                        expires_at: now + m.ttl,
                        viewer: m.viewer,
                        npc: m.npc,
                        think: m.think,
                        restore_hp: m.hp,
                    });
                }
            } else if now >= a.deadline {
                blow_out_candle();
                self.note = Some(
                    crate::i18n::t("spawn: the engine never took the request - nothing was created")
                        .to_string(),
                );
                if a.buddy_trigger != 0 {
                    // `req` всё ещё наш - движок поле не читал; ноль без
                    // прибавки пеплов - прочитал и отказал.
                    self.place_note = format!(
                        "timeout: req {} (sent {}) | active {} | buddies {}",
                        world.summon_buddy_manager.request_summon_speffect_id,
                        a.buddy_trigger,
                        world.summon_buddy_manager.active_summon_speffect_id,
                        buddy_handles(world).len(),
                    );
                }
                refunds.extend(self.awaiting.take().map(|a| (a.reward_id, a.redemption_id)));
            }
        }
        // Собираем локально: `retain_mut` уже держит `self.tracked`, и позвать
        // отсюда `self.remember_ghost` нельзя.
        let mut ghosts: Vec<(FieldInsHandle, String)> = Vec::new();
        self.tracked.retain_mut(|t| {
            // Врага мог убить игрок, а могла выгрузить сама игра при переходе
            // между локациями. Тогда следить больше не за кем, и слот лимита
            // обязан освободиться сразу - иначе убитый враг держал бы его до
            // конца TTL, и следующая покупка вернулась бы баллами впустую.
            //
            // Но не раньше `SETTLE_GRACE`: только что созданное существо ещё
            // не доинициализировано, и «мёртвым» оно выглядит просто потому,
            // что движок не успел.
            if now.duration_since(t.born) < SETTLE_GRACE {
                // Союзник - исключение из правила «в этом окне не трогаем
                // ничего»: без ранней записи первые четыре секунды призванный
                // дерётся с игроком (жалоба живьём 2026-09-07, подтверждена
                // повторно 2026-09-08).
                // Призванному системой команду ставит она сама, и врагу
                // достаётся дружественная - живьём Ложная Слеза приходила
                // союзной, кем бы её ни назначили. Перебиваем и у врага.
                if t.ally || t.summoned {
                    tend_spawn(world, t, now);
                }
                // Врага не трогаем вовсе - ни `chr_of`, ни `wake_up`. Движок
                // в это время дособирает существо, и наши касания тут стоили
                // трёх зависаний подряд.
                return true;
            }
            // Здоровье переносится через пересборку: пепел призывается заново
            // на каждую покупку, и избитый враг не должен вернуться целым.
            // Пишем здесь, а не при усыновлении: в окне сборки любое касание
            // существа трижды подряд вешало игру.
            if t.restore_hp > 0 {
                if let Some(chr) = chr_of(world, t) {
                    chr.modules.data.hp = t.restore_hp.min(chr.modules.data.max_hp.max(1));
                }
                t.restore_hp = 0;
            }
            // Приговор, один раз на запись. До него никаких проверок «а жив
            // ли» не было вовсе - иначе недособранное существо считалось бы
            // мёртвым.
            if !t.confirmed {
                t.confirmed = true;
                light_candle(t.key, "judge");
                if let Some(chr) = chr_of(world, t) {
                    // Нулевая команда значит «никто ему не противник», и
                    // только её мы и перебиваем: проставленная движком вернее
                    // нашей. Союзнику команду пишет `write_team` - поверх
                    // любой и не один раз.
                    //
                    // Побудки здесь больше нет: она писала в `debug_flags`,
                    // а его смещение в 2.7 уехало, и запись вешала игру.
                    if chr.team_type == 0 && !t.ally {
                        chr.team_type = t.team;
                    }
                }
                // Цена в рунах - у самого существа: призванный своим пеплом
                // строки парама в заявке не имеет вовсе. Резолв синглтона тут
                // не в счёт - приговор выносится один раз за спавн, а не
                // каждый кадр.
                if !t.ally {
                    let npc = chr_of(world, t).map_or(0, |c| c.npc_param_id);
                    t.souls = u32::try_from(npc)
                        .ok()
                        .and_then(|id| unsafe { SoloParamRepository::instance() }.ok()?.get::<NpcParam>(id))
                        .map_or(0, |r| r.get_soul().max(0) as u32);
                }
                // Слеза - копия игрока, и растёт вместе с ним. Остальных это
                // не касается: у них свои статы, а не наши.
                if own_ash(t.key) {
                    let level = world.main_player.as_ref().map_or(0, |p| unsafe {
                        p.player_game_data.as_ref().level
                    });
                    // Строку правим один раз, а вешаем каждому: в блоке Слезы
                    // два существа - сгусток и сам двойник, - и приговор они
                    // получают порознь.
                    if self.doping_hold.is_empty() {
                        if let Ok(repo) = unsafe { SoloParamRepository::instance_mut() } {
                            self.doping_hold = scale_mimic(repo, level);
                        }
                    }
                    let ids: Vec<u32> = self.doping_hold.iter().map(|(id, _)| *id).collect();
                    // Вешаем все: какая ступень улучшения достанется призыву,
                    // решает движок, а лишний множитель на несработавшей строке
                    // никого не трогает.
                    if let Some(chr) = chr_of(world, t) {
                        for id in ids {
                            chr.apply_speffect(id.cast_signed(), true);
                        }
                    }
                }
                let paid = !t.redemption_id.is_empty();
                if !took(world, t) {
                    // Без модели оно висит невидимым, с хитпоинтами и без
                    // поведения. Убираем и возвращаем баллы: зритель заплатил
                    // за врага, а получил пустое место.
                    //
                    // Различаем два случая: тело есть, но без модели - или
                    // тела нет вовсе. Снаружи и то и другое выглядит как
                    // «ничего не появилось», а причины разные.
                    let v = match chr_of(world, t) {
                        Some(_) => crate::i18n::t("spawn: the body was created but has no model - removed, points refunded"),
                        None => crate::i18n::t("spawn: no body - the engine created it and lost it at once"),
                    }
                    .to_string();
                    self.note = Some(v.clone());
                    self.verdict = Some(v);
                    unload(world, t);
                    if paid {
                        refunds.push((t.reward_id.clone(), t.redemption_id.clone()));
                    }
                    return false;
                }
                let v = crate::i18n::t("spawn: it worked").to_string();
                // Куда существо в итоге встало - в отладочное окно, а не в
                // строку для человека.
                if let Some(chr) = chr_of(world, t) {
                    let p = chr.modules.physics.position;
                    let hh = chr.modules.physics.hit_height;
                    self.place_note.push_str(&format!(
                        " || y {:.1} hh {hh:.1} stand {} fall {} want {:.1}",
                        p.1,
                        u8::from(chr.modules.physics.standing_on_solid_ground),
                        u8::from(chr.modules.physics.is_falling),
                        t.anchor.1,
                    ));
                }
                self.note = Some(v.clone());
                self.verdict = Some(v);
                light_candle(t.key, "live");
                if paid {
                    fulfilled.push((t.reward_id.clone(), t.redemption_id.clone()));
                }
            }
            // Свеча гаснет не в приговоре, а спустя `CANDLE_LINGER`: движок
            // вешается и ПОСЛЕ того, как отдал существо, - живьём 2026-08-28
            // игра встала уже с погашенной свечой, и кто это был, спросить
            // стало не у кого.
            if now.duration_since(t.born) >= CANDLE_LINGER {
                blow_out_candle();
            }
            if !alive(world, t) {
                // Платим только за убитого: за духа игра рун не даёт, а по
                // сроку и кнопкой существо снимает мод - игрок его не трогал.
                // Второй раз не начислим: запись тут же исчезает.
                if t.souls > 0 && killed(world, t) {
                    award_runes(world, t.souls);
                }
                ghosts.push((t.handle, t.viewer.clone()));
                return false;
            }
            if t.ally || t.summoned {
                tend_spawn(world, t, now);
                // Снимаем здесь, а не отдельным проходом: `chr_of` уже под
                // рукой, а резолвить существо второй раз за кадр незачем.
                // Показания - только у союзников: оверлей про них, врагу в нём
                // делать нечего.
                if let Some(chr) = chr_of(world, t).filter(|_| t.ally) {
                    let d = &chr.modules.data;
                    let (hp, max_hp) = (d.hp.max(0) as f32, d.max_hp.max(1) as f32);
                    let shown = lag_value(t.lag_from, hp, now.duration_since(t.lag_at));
                    // Здоровье упало - хвост начинается оттуда, где он сейчас
                    // нарисован. Тогда подряд идущие удары продолжают одну
                    // полосу, а не сбрасывают её к максимуму.
                    if hp < shown {
                        t.lag_from = shown;
                        t.lag_at = now;
                    } else if hp > shown {
                        // Подлечили - хвоста нет вовсе.
                        t.lag_from = hp;
                        t.lag_at = now;
                    }
                    t.view = Some(AllyView {
                        viewer: t.viewer.clone(),
                        name: label(t.key),
                        hp: hp as u32,
                        max_hp: max_hp as u32,
                        lag: (shown.max(hp) / max_hp).clamp(0.0, 1.0),
                        secs_left: t.expires_at.saturating_duration_since(now).as_secs_f32(),
                    });
                }
            }
            if now < t.expires_at {
                return true;
            }
            unload(world, t);
            false
        });
        for (handle, viewer) in ghosts {
            self.remember_ghost(handle, viewer);
        }
        // Слеза ушла - усиление возвращаем как было: строка общая для всех
        // пеплов, и держать её переписанной дольше нужного незачем.
        if !self.doping_hold.is_empty() {
            let mimic_left = self
                .tracked
                .iter()
                .any(|t| own_ash(t.key));
            if !mimic_left {
                if let Ok(repo) = unsafe { SoloParamRepository::instance_mut() } {
                    for (id, was) in std::mem::take(&mut self.doping_hold) {
                        restore_doping(repo, id, was);
                    }
                }
            }
        }
        if let Some((at, rows)) = self.ai_hold.as_ref() {
            if now.duration_since(*at) >= BUDDY_ROW_HOLD {
                if let Ok(repo) = unsafe { SoloParamRepository::instance_mut() } {
                    for (row, was) in rows {
                        if let Some(r) = repo.get_mut::<NpcThinkParam>(*row) {
                            r.set_is_buddy_ai(*was);
                        }
                    }
                    self.ai_hold = None;
                }
            }
        }
        (refunds, fulfilled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Слеза растёт ступенями по десять уровней, и сопротивление никогда не
    /// доходит до неуязвимости.
    ///
    /// Верхний предел тут не украшение: на пятидесяти ступенях множитель
    /// входящего урона вышел бы в ноль, а дальше в минус - то есть удары
    /// начали бы её лечить.
    #[test]
    fn level_bonus_grows_by_tens_and_never_reaches_invulnerability() {
        assert_eq!(level_bonus(0), (1.0, 1.0), "нулевой уровень ничего не меняет");
        assert_eq!(level_bonus(9), (1.0, 1.0), "неполная десятка не считается");

        let (atk, cut) = level_bonus(100);
        assert!((atk - 1.30).abs() < 1e-5, "десять ступеней - плюс 30% урона, вышло {atk}");
        assert!((cut - 0.80).abs() < 1e-5, "и минус 20% входящего, вышло {cut}");

        assert!(level_bonus(9999).1 >= 0.2, "сопротивление упирается в предел");
        assert!(level_bonus(9999).0 > level_bonus(100).0, "урон растёт и дальше");
    }

    /// Первая запись идёт сразу, дальше не чаще `ALLY_REASSERT`, и через
    /// `ALLY_REASSERT_FOR` повторы прекращаются совсем.
    ///
    /// Каждая половина закрывает свою поломку: без первой союзник дрался бы с
    /// игроком до приговора, без второй мод писал бы в живую структуру каждые
    /// полсекунды до конца TTL.
    #[test]
    fn the_ally_team_is_rewritten_early_often_and_then_never() {
        let born = Instant::now();
        assert!(due_for_team_write(born, None, born), "первая запись - сразу");

        let just_wrote = born + Duration::from_millis(100);
        assert!(
            !due_for_team_write(born, Some(just_wrote), just_wrote + ALLY_REASSERT / 2),
            "чаще ALLY_REASSERT не пишем"
        );
        assert!(
            due_for_team_write(born, Some(just_wrote), just_wrote + ALLY_REASSERT),
            "через ALLY_REASSERT - пора"
        );

        let late = born + ALLY_REASSERT_FOR + Duration::from_secs(1);
        assert!(!due_for_team_write(born, None, late), "окно повторов кончилось");
    }

    /// Хвост урона стоит на месте, потом стекает к текущему здоровью и там
    /// остаётся. Без выдержки удар не успевали бы заметить, без стекания
    /// бледная полоса висела бы до конца боя.
    #[test]
    fn the_damage_tail_holds_then_drains() {
        let secs = Duration::from_secs_f32;
        // Было 100, стало 40.
        assert_eq!(lag_value(100.0, 40.0, secs(0.0)), 100.0, "сразу после удара - на месте");
        assert_eq!(lag_value(100.0, 40.0, secs(LAG_HOLD)), 100.0, "всю выдержку - на месте");

        let mid = lag_value(100.0, 40.0, secs(LAG_HOLD + LAG_DRAIN * 0.5));
        assert!(mid > 40.0 && mid < 100.0, "на середине стекания: {mid}");

        assert_eq!(lag_value(100.0, 40.0, secs(LAG_HOLD + LAG_DRAIN)), 40.0, "стёк до текущего");
        assert_eq!(lag_value(100.0, 40.0, secs(100.0)), 40.0, "и ниже не уходит");
    }

    #[test]
    fn random_offset_stays_within_the_ring() {
        let mut state = 12345u32;
        for radius in [1.0, 5.0, 30.0] {
            for _ in 0..200 {
                let (dx, dz) = random_offset(radius, &mut state);
                let dist = (dx * dx + dz * dz).sqrt();
                assert!(dist <= radius + 0.001, "dist {dist} > radius {radius}");
                assert!(dist >= radius * MIN_DIST_FRACTION - 0.001, "dist {dist} < min for radius {radius}");
            }
        }
    }

    /// Перед взглядом - значит перед взглядом: точка обязана лежать по ту же
    /// сторону, что и направление камеры, и на заданном расстоянии.
    #[test]
    fn front_offset_lands_ahead_of_the_camera() {
        let mut state = 7u32;
        for forward in [(0.0f32, 1.0f32), (1.0, 0.0), (-1.0, 0.0), (0.7, -0.7)] {
            let len = (forward.0 * forward.0 + forward.1 * forward.1).sqrt();
            let dir = (forward.0 / len, forward.1 / len);
            for _ in 0..50 {
                let (dx, dz) = front_offset(forward, 6.0, &mut state).expect("направление задано");
                let dist = (dx * dx + dz * dz).sqrt();
                assert!((dist - 6.0).abs() < 0.001, "расстояние {dist} вместо 6");
                // Скалярное произведение с направлением взгляда: при разбросе
                // в 12 градусов оно обязано остаться заметно положительным.
                let ahead = (dx * dir.0 + dz * dir.1) / dist;
                assert!(ahead > 0.9, "точка ушла вбок или назад: {ahead}");
            }
        }
        // Взгляд строго вверх или вниз: горизонтального «вперёд» нет вовсе.
        assert_eq!(front_offset((0.0, 0.0), 6.0, &mut state), None);
    }

    /// Гость, улетевший в пустоту, опознаётся высотой, а спрыгнувший с уступа
    /// - нет. И ставим его на пол плюс поправка на рост: без неё крупные уходят
    /// по пояс в землю.
    #[test]
    fn a_guest_is_only_rescued_when_it_is_really_gone() {
        assert!(fell_below(6.2, -20.0));
        assert!(!fell_below(6.2, 3.0), "прыжок с уступа - не падение под карту");
        let floor = HavokPosition(1.0, 6.2, -3.0, 0.0);
        assert_eq!(target_y(floor, 1.8).1, 6.2, "человекоподобного не поднимаем");
        assert!(target_y(floor, 8.0).1 > 6.2, "крупного - поднимаем");
    }

    /// Отступ от края отодвигает точку ПО ТОЙ ЖЕ прямой и ровно на заданное
    /// расстояние: иначе проверка «а есть ли пол чуть дальше» спрашивала бы
    /// не про то место.
    #[test]
    fn push_out_extends_the_same_direction() {
        let player = HavokPosition(10.0, 5.0, -3.0, 0.0);
        for (x, z) in [(18.0f32, -3.0f32), (10.0, 5.0), (4.0, -9.0)] {
            let (ex, ez) = push_out(player, x, z, 0.75);
            let before = ((x - player.0).powi(2) + (z - player.2).powi(2)).sqrt();
            let after = ((ex - player.0).powi(2) + (ez - player.2).powi(2)).sqrt();
            assert!((after - before - 0.75).abs() < 0.001, "отодвинули на {}", after - before);
            // Та же прямая: векторное произведение исходного и нового = 0.
            let cross = (x - player.0) * (ez - player.2) - (z - player.2) * (ex - player.0);
            assert!(cross.abs() < 0.001, "точка ушла с прямой");
        }
        // Точка ровно под игроком - направления нет, отодвигать некуда.
        assert_eq!(push_out(player, player.0, player.2, 0.75), (player.0, player.2));
    }

    #[test]
    fn random_offset_is_not_constant() {
        let mut state = 1u32;
        let first = random_offset(10.0, &mut state);
        let second = random_offset(10.0, &mut state);
        assert_ne!(first, second, "последовательные вызовы обязаны давать разные точки");
    }

    #[test]
    fn xorshift_recovers_from_a_zero_seed() {
        let mut state = 0u32;
        let first = xorshift32(&mut state);
        assert_ne!(first, 0, "нулевой seed не должен залипать навсегда");
    }

    /// `chr_id` - это первые четыре цифры param-id, и из него движок собирает
    /// имя ассета. Ошибка здесь = запрос несуществующей модели.
    #[test]
    fn chr_id_is_the_leading_four_digits() {
        let skeleton = entry("skeleton").expect("скелет в таблице");
        assert_eq!(skeleton.npc_param_id, 35001010);
        assert_eq!(skeleton.chr_id(), 3500);
        // Числа из таблицы спавна streamtoearn: там `chr_id` указан отдельно,
        // и он совпал с вычисленным на всех взятых строках.
        assert_eq!(entry("malenia").unwrap().chr_id(), 2120);
        assert_eq!(entry("runebear").unwrap().chr_id(), 4630);
        assert_eq!(entry("living_jar").unwrap().chr_id(), 4491);
    }

    /// Поиск и фильтр - единственный способ найти нужного в списке под две
    /// сотни строк. Ищем и по переводу, и по английскому названию, и по ключу:
    /// подпись стоит на языке интерфейса, а знакомое имя из вики человек
    /// набирает как помнит.
    #[test]
    fn search_matches_the_name_the_key_and_the_boss_filter() {
        let malenia = entry("malenia").unwrap();
        assert!(malenia.matches("", false), "пустой запрос показывает всех");
        assert!(malenia.matches("MALEN", false), "по названию, без учёта регистра");
        assert!(malenia.matches("malenia", false), "и по ключу из файла наград");
        assert!(malenia.matches("", true), "Маления - босс");

        let noble = entry("noble").unwrap();
        assert!(!noble.matches("", true), "обычный враг под фильтром боссов не показывается");
        assert!(!noble.matches("malenia", false), "чужое имя не должно совпадать");
    }

    /// Хоть один босс и хоть один обычный: с пустой половиной фильтр
    /// показывал бы пустоту.
    #[test]
    fn table_has_both_bosses_and_regulars() {
        assert!(SPAWN_TABLE.iter().any(|e| e.boss()));
        assert!(SPAWN_TABLE.iter().any(|e| !e.boss()));
    }

    /// Все id взяты из парамов игры и обязаны иметь одну форму: восемь цифр,
    /// из которых первые четыре - существующая модель. Из них выводится
    /// `chr_id`, а ошибка в нём - это запрос несуществующего ассета.
    ///
    /// Девятизначных строк парама тут быть не должно: они принадлежат
    /// существам без собственной модели (двойники, призывы, уникальные NPC),
    /// и наше правило `id / 10 000` дало бы для них пятизначный номер.
    #[test]
    fn spawn_table_ids_are_well_formed() {
        for e in SPAWN_TABLE {
            // У призываемого своим пеплом строки парама нет вовсе - и не
            // должно быть: его собирает игра, а не мы.
            if e.ash != 0 {
                assert_eq!(e.npc_param_id, 0, "{}: свой пепел и param-id разом", e.key);
                continue;
            }
            // У двойника номер строки из чужого диапазона, и модель у него
            // не своя - правило `id / 10 000` к нему неприменимо.
            if e.init != 0 {
                assert!(e.think != 0, "{}: двойник без строки поведения", e.key);
                continue;
            }
            assert!(
                (10_000_000..100_000_000).contains(&e.npc_param_id),
                "{}: {} не похож на NpcParam id",
                e.key,
                e.npc_param_id
            );
            assert!((1000..10_000).contains(&e.chr_id()), "{}: chr_id {} вне диапазона", e.key, e.chr_id());
        }
    }

    #[test]
    fn spawn_table_keys_are_unique() {
        for (i, e) in SPAWN_TABLE.iter().enumerate() {
            for other in &SPAWN_TABLE[i + 1..] {
                assert_ne!(e.key, other.key, "ключ {} в таблице дважды", e.key);
            }
        }
    }

    #[test]
    fn key_of_and_entry_roundtrip() {
        let first = SPAWN_TABLE[0].key;
        assert_eq!(key_of(first), Some(first));
        assert!(entry(first).is_some());
    }

    #[test]
    fn unknown_key_is_none() {
        assert_eq!(key_of("нет-такого-ключа"), None);
        assert!(entry("нет-такого-ключа").is_none());
    }

    /// Лимита одинаковых больше нет (удалён по запросу 2026-09-07): явный
    /// выбор врага ничем не ограничен. Ограничение осталось только у лотереи -
    /// его держит `random_reward_never_repeats_itself`.
    #[test]
    fn the_same_enemy_can_be_picked_twice_on_purpose() {
        let mut st = SpawnState::default();
        let one = |st: &mut SpawnState, key| {
            st.schedule(key, Duration::ZERO, Duration::from_secs(30), 10, "r", "p", "v", false)
        };
        assert!(one(&mut st, "skeleton").is_ok());
        assert!(one(&mut st, "skeleton").is_ok());
    }

    /// У каждой ступени обязан быть хоть один враг: пустая - это награда
    /// «случайный лёгкий», которая всегда отбивается отказом.
    #[test]
    fn every_tier_has_someone() {
        for tier in [Tier::Easy, Tier::Normal, Tier::Hard, Tier::Boss] {
            assert!(SPAWN_TABLE.iter().any(|e| e.tier == tier), "{tier:?} пуста");
        }
    }

    /// Псевдо-ключ не должен совпасть с настоящим: `entry` смотрится первой, и
    /// такой враг стал бы недоступен, а награда - неслучайной.
    #[test]
    fn random_keys_do_not_clash_with_the_table() {
        for p in RANDOM_PICKS {
            assert!(entry(p.key).is_none(), "{} есть и в таблице", p.key);
            // И `decode` в rewards.rs обязан их принимать, иначе награда со
            // случайным врагом не переживёт перезапуск игры.
            assert_eq!(key_of(p.key), Some(p.key));
            assert_ne!(label(p.key), p.key, "подписи нет - в списке будет виден ключ");
        }
    }

    /// Поиск по ступени: «I» отдаёт только лёгких и не цепляет III/IV.
    #[test]
    fn search_by_roman_numeral_picks_one_tier() {
        for (needle, tier) in [("i", Tier::Easy), ("II", Tier::Normal), ("iii", Tier::Hard), ("IV", Tier::Boss)] {
            let hits: Vec<&SpawnEntry> = SPAWN_TABLE.iter().filter(|e| e.matches(needle, false)).collect();
            assert!(!hits.is_empty(), "{needle}: пусто");
            assert!(hits.iter().all(|e| e.tier == tier), "{needle}: попали чужие ступени");
        }
    }

    /// «Случайный: лёгкий» выдаёт настоящих врагов своей ступени и **без
    /// повторов**: при лимите одинаковых в единицу тот же враг второй раз не
    /// выйдет, пока первый не исчез.
    /// Уронивший игру в прошлый запуск в лотерею больше не идёт: иначе
    /// «случайный враг» роняет стрим по кругу, пока стример не догадается,
    /// кто именно виноват.
    #[test]
    fn a_blamed_enemy_never_comes_up_at_random() {
        let victim = SPAWN_TABLE.iter().find(|e| e.tier == Tier::Boss).unwrap().key;
        blamed_mut().push(victim);
        let mut st = SpawnState::default();
        for _ in 0..500 {
            let Ok(got) = st.roll("random_boss") else { panic!("боссов больше одного") };
            assert_ne!(got, victim, "обвинённый выпал в случайной награде");
        }
        forgive();
    }

    #[test]
    fn random_reward_never_repeats_itself() {
        let mut st = SpawnState::default();
        let easy = SPAWN_TABLE.iter().filter(|e| e.tier == Tier::Easy).count();
        for _ in 0..easy {
            st.schedule("random_easy", Duration::ZERO, Duration::from_secs(30), 999, "r", "p", "v", false)
                .unwrap_or_else(|_| panic!("лёгкие ещё не кончились"));
        }
        let picked: Vec<&str> = st.delayed.iter().map(|d| d.key).collect();
        assert_eq!(picked.len(), easy);
        for key in &picked {
            let e = entry(key).expect("выпал настоящий враг");
            assert_eq!(e.tier, Tier::Easy, "{key} не лёгкий");
            assert_eq!(picked.iter().filter(|k| *k == key).count(), 1, "{key} выпал дважды");
        }
        // Свободных больше нет - отказ, а не повтор.
        assert!(matches!(
            st.schedule("random_easy", Duration::ZERO, Duration::from_secs(30), 999, "r", "p", "v", false),
            Err(SpawnRejected::SameLimitReached)
        ));
    }

    /// «Случайный босс» не должен выдать рядового.
    #[test]
    fn random_boss_rolls_only_bosses() {
        let mut st = SpawnState::default();
        for _ in 0..20 {
            st.schedule("random_boss", Duration::ZERO, Duration::from_secs(30), 999, "r", "p", "v", false)
                .unwrap_or_else(|_| panic!("боссов в таблице много"));
        }
        for d in &st.delayed {
            assert!(entry(d.key).expect("настоящий враг").boss(), "{} не босс", d.key);
        }
    }

    #[test]
    fn slot_available_respects_limit_and_awaiting() {
        assert!(slot_available(0, false, 3));
        assert!(slot_available(2, false, 3));
        assert!(!slot_available(3, false, 3));
        // Заявка в полёте занимает слот, даже пока движок ещё не подтвердил
        // спавн - иначе быстрые повторные покупки проскочили бы лимит.
        assert!(!slot_available(2, true, 3));
        assert!(slot_available(1, true, 3));
        // Лимит 0 трактуется как 1 - то же правило, что у `enqueue` в actions.rs.
        assert!(slot_available(0, false, 0));
        assert!(!slot_available(1, false, 0));
    }
}
