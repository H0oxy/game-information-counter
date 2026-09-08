//! Что зритель может купить: список действий в отдельном файле.
//!
//! Не в `.ini`: `Config` держит только скаляры, а тут список произвольной
//! длины. Файл `game_information_counter.rewards` лежит рядом с DLL и устроен как
//! `.stats` - строка на запись, битая строка пропускается, а не роняет
//! загрузку (файл правится руками).
//!
//! Награды создаёт стример на дашборде Twitch, мод их только слушает. Здесь
//! хранится ровно связка «награда -> что сделать в игре».

use hudhook::imgui::Key;

use crate::config::{dll_sibling, parse_key};

const REWARDS_FILE: &str = "game_information_counter.rewards";

/// Сколько живёт призванный, если у награды срок не выставлен. Столько же
/// стояло в `spawn_ttl_secs`, пока настройка была общей.
pub const DEFAULT_SPAWN_TTL_SECS: u16 = 180;

/// То же для союзника. Втрое короче: союзник приходит помочь в драке, а не
/// населять мир, и три минуты помощи - это уже не помощь (запрос 2026-09-09).
pub const DEFAULT_ALLY_TTL_SECS: u16 = 60;

/// Срок по умолчанию для того, кого призывают.
pub const fn default_ttl_secs(ally: bool) -> u16 {
    if ally { DEFAULT_ALLY_TTL_SECS } else { DEFAULT_SPAWN_TTL_SECS }
}

/// Что делать, когда награду погасили.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    /// Держать клавишу заданное время.
    Hold { key: Key, duration_ms: u32 },
    /// Короткое нажатие.
    Press { key: Key },
    /// Заспавнить врага из куратор-списка (`crate::spawn::SPAWN_TABLE`) рядом
    /// с игроком. `&'static str`, а не `String` - `Action` остаётся `Copy`.
    ///
    /// Ключ бывает и псевдо-ключом «случайный» (`crate::spawn::RANDOM_PICKS`):
    /// тогда конкретное существо выбирается в момент покупки.
    ///
    /// `delay_secs` - через сколько появится: у босса это время зрителям
    /// поглядеть на отсчёт, а стримеру - добежать до удобного места.
    ///
    /// `cooldown_secs` - то же самое, что у `Effect`: перезарядка этой награды
    /// для всех разом.
    ///
    /// Времени жизни здесь НЕТ: оно одно на все спавны и живёт в `.ini`
    /// (`spawn_ttl_secs`). Своё поле у награды дублировало ту же настройку и
    /// вдобавок не работало (жалоба 2026-09-03).
    SpawnEnemy { key: &'static str, delay_secs: u16, cooldown_secs: u16, ally: bool, ttl_secs: u16 },
    /// Применить эффект из `crate::effects::EFFECT_TABLE` к игроку или к его
    /// цели. `&'static str` по той же причине, что у спавна - `Action`
    /// остаётся `Copy`.
    ///
    /// `secs` - сколько эффект держится, `0` = взять срок из таблицы. У
    /// разовых (лечение, +3 фляги) смысла не имеет и в UI не показывается.
    ///
    /// `in_boss` - исполнять ли покупку, пока идёт бой с боссом. Своя галочка
    /// у каждой награды, а не общая настройка: «подлечить босса» ради боя и
    /// покупают, а «забрать фляги» посреди Малении - это испорченный ран, и
    /// решает тут стример.
    ///
    /// `cooldown_secs` - сколько после покупки эту же награду нельзя купить
    /// снова, `0` - без перезарядки (прямой запрос 2026-08-24). Это
    /// перезарядка ЭТОЙ награды для всех разом, кто бы ни платил; соседние
    /// награды и другие эффекты она не трогает. Единственная в моде: прежние
    /// «паузы на зрителя» по категориям (`viewer_cooldown_*_secs` в `.ini`)
    /// были тем же самым с другой стороны и удалены как дубль
    /// (запрос 2026-09-03).
    ///
    /// Пока она идёт, награда ВЫКЛЮЧЕНА на дашборде Twitch - иначе зритель
    /// видел бы только молчаливый возврат баллов и не понимал, почему.
    Effect { key: &'static str, secs: u16, in_boss: bool, cooldown_secs: u16 },
}

impl Action {
    /// Клавиша для `SpawnEnemy` - плейсхолдер, никогда не пишется обратно в
    /// файл (см. `encode`). Нужен только чтобы UI не заводил `Option<Key>`
    /// через весь `settings.rs` ради одного варианта из трёх.
    pub fn key(&self) -> Key {
        match self {
            Action::Hold { key, .. } | Action::Press { key } => *key,
            Action::SpawnEnemy { .. } | Action::Effect { .. } => Key::Space,
        }
    }

    /// Категория действия: у каждой своя пауза на зрителя в `.ini`. Три, а не
    /// четыре: нажатие и удержание - одно и то же действие с разной длиной, и
    /// в интерфейсе они давно один пункт.
    pub fn category(&self) -> &'static str {
        match self {
            Action::Hold { .. } | Action::Press { .. } => "key",
            Action::SpawnEnemy { .. } => "spawn",
            Action::Effect { .. } => "effect",
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Action::Hold { .. } => "hold",
            Action::Press { .. } => "press",
            Action::SpawnEnemy { .. } => "spawn",
            Action::Effect { .. } => "effect",
        }
    }

    /// Пятое поле строки в файле. У удержания это миллисекунды нажатия, у
    /// эффекта - его срок, у спавна - сколько врагу жить.
    ///
    /// У спавна оно уже значило ровно это до 2026-09-03, когда своё время
    /// жизни у награды убрали как дубликат общей настройки. Вернулось
    /// 2026-09-07 - но теперь это ЕДИНСТВЕННОЕ место, где срок задаётся, и
    /// дублировать больше нечего. Старые строки от этого чинятся сами.
    fn duration_ms(&self) -> u32 {
        match self {
            Action::Hold { duration_ms, .. } => *duration_ms,
            Action::Effect { secs, .. } => u32::from(*secs) * 1000,
            Action::SpawnEnemy { ttl_secs, .. } => u32::from(*ttl_secs) * 1000,
            Action::Press { .. } => 0,
        }
    }

    /// Сколько жить призванному, секунды. Ноль в файле - строка прошлой версии
    /// (или срок не выставляли вовсе), тогда берём умолчание.
    pub fn spawn_ttl_secs(&self) -> u16 {
        match self {
            Action::SpawnEnemy { ttl_secs: 0, ally, .. } => default_ttl_secs(*ally),
            Action::SpawnEnemy { ttl_secs, .. } => *ttl_secs,
            _ => DEFAULT_SPAWN_TTL_SECS,
        }
    }

    /// Нужна ли этой покупке настоящая клавиатура.
    ///
    /// Только нажатию и удержанию: `SendInput` бьёт по активному окну всей
    /// системы, и его перехватывает наше же окно настроек. Спавн и эффект
    /// пишут игровую память напрямую, им ни то, ни другое не мешает.
    pub fn needs_keyboard(&self) -> bool {
        matches!(self, Action::Press { .. } | Action::Hold { .. })
    }

    /// Призвать существо на своей стороне (команда «дух-призыв»), а не врагом.
    pub fn spawns_ally(&self) -> bool {
        matches!(self, Action::SpawnEnemy { ally: true, .. })
    }

    pub fn spawn_delay(&self) -> u16 {
        match self {
            Action::SpawnEnemy { delay_secs, .. } => *delay_secs,
            _ => 0,
        }
    }

    /// Перезарядка НАГРАДЫ (не зрителя), `0` - выключена. Есть у эффектов и у
    /// спавна - см. `Action::Effect`.
    pub fn cooldown_secs(&self) -> u16 {
        match self {
            Action::Effect { cooldown_secs, .. } | Action::SpawnEnemy { cooldown_secs, .. } => *cooldown_secs,
            _ => 0,
        }
    }

    /// Можно ли исполнять эту покупку в бою с боссом. У всего, кроме эффектов,
    /// вопрос не стоит: клавишу и спавн разбирают свои правила.
    pub fn allowed_in_boss(&self) -> bool {
        match self {
            Action::Effect { in_boss, .. } => *in_boss,
            _ => true,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct RewardEntry {
    /// Локальный номер записи, не имеющий отношения к Twitch. Нужен, чтобы UI
    /// адресовал строку независимо от её места в списке.
    pub id: u32,
    pub enabled: bool,
    pub action: Action,
    /// Цена - только для показа. Списывает баллы сам Twitch, и он же не даёт
    /// зрителю нажать «Погасить» без нужной суммы: мод баланс не видит и
    /// проверять его не должен.
    pub cost: u32,
    /// UUID награды. Если пусто - сопоставляем по названию.
    pub reward_id: String,
    /// Название награды на Twitch. Основной способ сопоставления: стример
    /// копирует его с дашборда, а не ищет UUID.
    pub reward_title: String,
    /// Что писать в карточке вместо названия. Пусто - берётся название.
    pub label: String,
    /// Отпечаток того, что последний раз уехало на Twitch (см. `fingerprint`).
    /// `0` - не уезжало ничего.
    ///
    /// Нужен ровно для одного: отличить «награда на Twitch и совпадает» от
    /// «награда на Twitch, но цену с тех пор поменяли». Без этого правка цены
    /// молча не доезжала до дашборда, и понять это было нельзя ничем
    /// (жалоба 2026-08-23).
    pub synced: u64,
}

/// Отпечаток полей, которые мод отправляет на Twitch.
///
/// FNV-1a, а не крипто-хэш: сравнивать надо только «то же самое или уже нет»,
/// и подделывать тут нечего - файл лежит у самого стримера.
/// Галочки тут нет намеренно: она на Twitch не уезжает (см.
/// `helix::update_reward`), и считать её «изменением» значило бы звать
/// обновлять то, что там и так совпадает.
pub fn fingerprint(title: &str, cost: u32) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: u8| {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    };
    for b in title.trim().as_bytes() {
        eat(*b);
    }
    eat(b'|');
    for b in cost.to_string().as_bytes() {
        eat(*b);
    }
    // Ноль означает «не синхронизировано» - на него отпечаток попасть не может.
    h.max(1)
}

impl RewardEntry {
    /// Совпадает ли то, что на Twitch, с тем, что настроено здесь.
    ///
    /// `None` - награды там нет вовсе.
    pub fn in_sync(&self) -> Option<bool> {
        if self.reward_id.is_empty() {
            return None;
        }
        Some(self.synced == fingerprint(&self.reward_title, self.cost))
    }

    /// Отпечаток текущих полей - его и запоминаем после удачной отправки.
    pub fn mark(&self) -> u64 {
        fingerprint(&self.reward_title, self.cost)
    }

    /// Что показать в карточке покупки.
    pub fn caption(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.reward_title
        } else {
            &self.label
        }
    }
}

/// Клавиши, которые мод умеет нажимать, вместе с кодом для Win32.
///
/// **Одна таблица на оба применения**: список для назначения и код для
/// `SendInput`. Раньше это были два разных списка в двух файлах, и они
/// разъехались - `actions::vk_of` знал все 26 букв, а назначить можно было 23
/// (жалоба 2026-08-19: «h, m и вся вторая половина клавиатуры не биндятся»).
/// Тест `the_whole_keyboard_is_bindable` держит их одним целым.
///
/// `Escape` сюда НЕ входит намеренно: им отменяется сам режим захвата, и
/// назначаемый Escape означал бы, что из захвата не выйти.
/// `KeypadEnter` тоже нет - у него тот же VK, что у обычного Enter, различить
/// их можно только скан-кодом, а Enter покрывает оба.
pub const KEYS: &[(Key, u16)] = &[
    // Буквы
    (Key::A, 0x41), (Key::B, 0x42), (Key::C, 0x43), (Key::D, 0x44),
    (Key::E, 0x45), (Key::F, 0x46), (Key::G, 0x47), (Key::H, 0x48),
    (Key::I, 0x49), (Key::J, 0x4A), (Key::K, 0x4B), (Key::L, 0x4C),
    (Key::M, 0x4D), (Key::N, 0x4E), (Key::O, 0x4F), (Key::P, 0x50),
    (Key::Q, 0x51), (Key::R, 0x52), (Key::S, 0x53), (Key::T, 0x54),
    (Key::U, 0x55), (Key::V, 0x56), (Key::W, 0x57), (Key::X, 0x58),
    (Key::Y, 0x59), (Key::Z, 0x5A),
    // Цифры верхнего ряда
    (Key::Alpha0, 0x30), (Key::Alpha1, 0x31), (Key::Alpha2, 0x32),
    (Key::Alpha3, 0x33), (Key::Alpha4, 0x34), (Key::Alpha5, 0x35),
    (Key::Alpha6, 0x36), (Key::Alpha7, 0x37), (Key::Alpha8, 0x38),
    (Key::Alpha9, 0x39),
    // Основные
    (Key::Space, 0x20), (Key::Enter, 0x0D), (Key::Tab, 0x09), (Key::Backspace, 0x08),
    // Модификаторы
    (Key::LeftShift, 0xA0), (Key::RightShift, 0xA1),
    (Key::LeftCtrl, 0xA2), (Key::RightCtrl, 0xA3),
    (Key::LeftAlt, 0xA4), (Key::RightAlt, 0xA5),
    // Стрелки
    (Key::UpArrow, 0x26), (Key::DownArrow, 0x28),
    (Key::LeftArrow, 0x25), (Key::RightArrow, 0x27),
    // Навигационный блок
    (Key::Insert, 0x2D), (Key::Delete, 0x2E), (Key::Home, 0x24),
    (Key::End, 0x23), (Key::PageUp, 0x21), (Key::PageDown, 0x22),
    // F-ряд
    (Key::F1, 0x70), (Key::F2, 0x71), (Key::F3, 0x72), (Key::F4, 0x73),
    (Key::F5, 0x74), (Key::F6, 0x75), (Key::F7, 0x76), (Key::F8, 0x77),
    (Key::F9, 0x78), (Key::F10, 0x79), (Key::F11, 0x7A), (Key::F12, 0x7B),
    // Цифровой блок
    (Key::Keypad0, 0x60), (Key::Keypad1, 0x61), (Key::Keypad2, 0x62),
    (Key::Keypad3, 0x63), (Key::Keypad4, 0x64), (Key::Keypad5, 0x65),
    (Key::Keypad6, 0x66), (Key::Keypad7, 0x67), (Key::Keypad8, 0x68),
    (Key::Keypad9, 0x69), (Key::KeypadDecimal, 0x6E), (Key::KeypadDivide, 0x6F),
    (Key::KeypadMultiply, 0x6A), (Key::KeypadSubtract, 0x6D), (Key::KeypadAdd, 0x6B),
    // Знаки. Коды раскладочные (OEM), но мод шлёт скан-код, а не VK, так что
    // на не-QWERTY клавиша остаётся на своём физическом месте.
    (Key::GraveAccent, 0xC0), (Key::Minus, 0xBD), (Key::Equal, 0xBB),
    (Key::LeftBracket, 0xDB), (Key::RightBracket, 0xDD), (Key::Backslash, 0xDC),
    (Key::Semicolon, 0xBA), (Key::Apostrophe, 0xDE), (Key::Comma, 0xBC),
    (Key::Period, 0xBE), (Key::Slash, 0xBF),
    // Залипающие и системные
    (Key::CapsLock, 0x14), (Key::NumLock, 0x90), (Key::ScrollLock, 0x91),
    (Key::PrintScreen, 0x2C), (Key::Pause, 0x13),
    (Key::LeftSuper, 0x5B), (Key::RightSuper, 0x5C), (Key::Menu, 0x5D),
];

/// Код клавиши для Win32. `None` - такой клавиши мод нажимать не умеет:
/// в файл наград руками можно вписать что угодно, и лучше не сделать ничего,
/// чем нажать случайное.
pub fn vk_of(key: Key) -> Option<u16> {
    KEYS.iter().find(|(k, _)| *k == key).map(|(_, vk)| *vk)
}

/// Клавиши, которые клавиатура шлёт с префиксом `0xE0`.
///
/// **Список явный, а не вычисленный.** `MapVirtualKeyW(.., MAPVK_VK_TO_VSC_EX)`
/// обещает вернуть `0xE0` в старшем байте, но для стрелок и навигационного
/// блока НЕ возвращает: замер 2026-08-19 дал у стрелки вверх `0x0048` - ровно
/// тот же скан-код, что у Num 8. Без флага игра и получала Num 8 вместо
/// стрелки (жалоба «стрелочки не нажимаются по кнопке проверки»).
pub const EXTENDED_VKS: &[u16] = &[
    0x25, 0x26, 0x27, 0x28, // стрелки
    0x21, 0x22, 0x23, 0x24, 0x2D, 0x2E, // PageUp/Down, End, Home, Insert, Delete
    0xA3, 0xA5, // правые Ctrl и Alt
    0x5B, 0x5C, 0x5D, // Win левый/правый, Menu
    0x90, 0x6F, 0x2C, // NumLock, Num /, PrintScreen
];

/// Человеческое имя клавиши: `LeftShift` читается хуже, чем «Shift (лев.)», а
/// стрелки без подписи вообще не отличить друг от друга.
pub fn key_label(key: Key) -> String {
    use crate::i18n::t;
    match key {
        Key::Space => t("Space").into(),
        Key::Tab => "Tab".into(),
        Key::Enter => "Enter".into(),
        Key::LeftShift => t("Left Shift").into(),
        Key::RightShift => t("Right Shift").into(),
        Key::LeftCtrl => t("Left Ctrl").into(),
        Key::RightCtrl => t("Right Ctrl").into(),
        Key::LeftAlt => t("Left Alt").into(),
        Key::RightAlt => t("Right Alt").into(),
        Key::UpArrow => t("Arrow Up").into(),
        Key::DownArrow => t("Arrow Down").into(),
        Key::LeftArrow => t("Arrow Left").into(),
        Key::RightArrow => t("Arrow Right").into(),
        Key::Backspace => "Backspace".into(),
        Key::Insert => "Insert".into(),
        Key::Delete => "Delete".into(),
        Key::Home => "Home".into(),
        Key::End => "End".into(),
        Key::PageUp => "Page Up".into(),
        Key::PageDown => "Page Down".into(),
        Key::CapsLock => "Caps Lock".into(),
        Key::NumLock => "Num Lock".into(),
        Key::ScrollLock => "Scroll Lock".into(),
        Key::PrintScreen => "Print Screen".into(),
        Key::Pause => "Pause".into(),
        Key::LeftSuper => t("Left Win").into(),
        Key::RightSuper => t("Right Win").into(),
        Key::Menu => t("Menu").into(),
        // Знаки препинания: `{:?}` дал бы "GraveAccent" вместо самого знака.
        Key::GraveAccent => "`".into(),
        Key::Minus => "-".into(),
        Key::Equal => "=".into(),
        Key::LeftBracket => "[".into(),
        Key::RightBracket => "]".into(),
        Key::Backslash => "\\".into(),
        Key::Semicolon => ";".into(),
        Key::Apostrophe => "'".into(),
        Key::Comma => ",".into(),
        Key::Period => ".".into(),
        Key::Slash => "/".into(),
        Key::KeypadDecimal => "Num .".into(),
        Key::KeypadDivide => "Num /".into(),
        Key::KeypadMultiply => "Num *".into(),
        Key::KeypadSubtract => "Num -".into(),
        Key::KeypadAdd => "Num +".into(),
        // `Alpha1` -> "1", `Keypad7` -> "Num 7", остальное как есть (буквы,
        // F-ряд): у них `{:?}` и так читается.
        other => {
            let name = format!("{other:?}");
            match name.strip_prefix("Alpha").or_else(|| name.strip_prefix("Keypad")) {
                Some(digit) if name.starts_with("Keypad") => format!("Num {digit}"),
                Some(digit) => digit.to_string(),
                None => name,
            }
        }
    }
}

pub fn key_name(key: Key) -> String {
    format!("{key:?}")
}

/// `|` в свободных полях сломал бы разбор, поэтому вычищается при записи.
/// Заодно от этого перестаёт быть важной позиция полей, и новые можно
/// дописывать в конец, не трогая уже написанные строки.
fn sanitize(s: &str) -> String {
    s.replace('|', " ").replace(['\r', '\n'], " ").trim().to_string()
}

fn encode(e: &RewardEntry) -> String {
    // Четвёртое поле означает разное в зависимости от типа (третье поле) -
    // так же, как пятое (`duration_ms`) осмысленно только для `hold`.
    let key_field = match &e.action {
        Action::SpawnEnemy { key, .. } | Action::Effect { key, .. } => key.to_string(),
        _ => key_name(e.action.key()),
    };
    format!(
        "reward = {}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        e.id,
        i32::from(e.enabled),
        e.action.kind(),
        key_field,
        e.action.duration_ms(),
        e.cost,
        sanitize(&e.reward_id),
        sanitize(&e.reward_title),
        sanitize(&e.label),
        // Девятое поле: у спавна это задержка, у эффекта - ЗАПРЕТ в бою с
        // боссом. Именно запрет, а не разрешение: ноль в уже написанных
        // строках должен читаться как «разрешено», то есть как было до этого
        // поля вовсе.
        match e.action {
            Action::Effect { in_boss, .. } => u16::from(!in_boss),
            other => other.spawn_delay(),
        },
        e.synced,
        // Двенадцатое поле: перезарядка награды-эффекта, `0` - выключена и у
        // всего остального, и у эффекта без неё. Хвостовое, как и задержка -
        // строки без него читаются как «без перезарядки», то есть как было
        // до этого поля вовсе.
        e.action.cooldown_secs(),
        // Тринадцатое поле: спавн на стороне игрока. Хвостовое, как задержка и
        // перезарядка - строки без него читаются как «врагом», то есть как
        // было до этого поля вовсе.
        i32::from(e.action.spawns_ally()),
    )
}

/// Битая строка даёт `None` и пропускается. Недостающие поля в конце - не
/// повод терять запись: их место занимают пустые значения, тем же приёмом, что
/// в `decode_attempt`.
fn decode(value: &str) -> Option<RewardEntry> {
    let p: Vec<&str> = value.split('|').collect();
    if p.len() < 6 {
        return None;
    }
    let id = p[0].trim().parse().ok()?;
    let enabled = p[1].trim() != "0";
    let duration_ms = p[4].trim().parse().unwrap_or(0);
    let action = match p[2].trim() {
        "hold" => Action::Hold { key: parse_key(p[3].trim())?, duration_ms },
        "press" => Action::Press { key: parse_key(p[3].trim())? },
        // Битый ключ куратор-списка (стёрли строку из SPAWN_TABLE, опечатка
        // руками) - строка не грузится, как и с неизвестной клавишей выше.
        "spawn" => Action::SpawnEnemy {
            key: crate::spawn::key_of(p[3].trim())?,
            // Хвостовые поля могут отсутствовать - строка из файла, который
            // писала прошлая версия мода, обязана читаться дальше. Пятое поле
            // (время жизни) у старых строк просто игнорируется.
            delay_secs: p.get(9).and_then(|v| v.trim().parse().ok()).unwrap_or(0),
            cooldown_secs: p.get(11).and_then(|v| v.trim().parse().ok()).unwrap_or(0),
            ally: p.get(12).is_some_and(|v| v.trim() == "1"),
            ttl_secs: (duration_ms / 1000).try_into().unwrap_or(0),
        },
        // Битый ключ эффекта - строка не грузится, как и с неизвестной
        // клавишей или врагом выше.
        "effect" => Action::Effect {
            key: crate::effects::key_of(p[3].trim())?,
            secs: (duration_ms / 1000).try_into().unwrap_or(0),
            // Хвостовое поле, и хранится в нём ЗАПРЕТ - см. `encode`.
            in_boss: p.get(9).and_then(|v| v.trim().parse::<u16>().ok()).unwrap_or(0) == 0,
            cooldown_secs: p.get(11).and_then(|v| v.trim().parse().ok()).unwrap_or(0),
        },
        _ => return None,
    };
    let field = |i: usize| p.get(i).map(|s| s.trim().to_string()).unwrap_or_default();
    Some(RewardEntry {
        id,
        enabled,
        action,
        cost: p[5].trim().parse().unwrap_or(0),
        reward_id: field(6),
        reward_title: field(7),
        label: field(8),
        // Хвостовое поле: у строки, написанной прошлой версией мода, его нет,
        // и ноль читается как «на Twitch ещё ничего не отправляли».
        synced: p.get(10).and_then(|v| v.trim().parse().ok()).unwrap_or(0),
    })
}

pub fn load(hmodule: usize) -> Vec<RewardEntry> {
    let Some(path) = dll_sibling(hmodule, REWARDS_FILE) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_all(&text)
}

fn parse_all(text: &str) -> Vec<RewardEntry> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            (key.trim() == "reward").then(|| decode(value))?
        })
        .collect()
}

const HEADER: &str = "\
; Game Information Counter - что зрители могут купить за баллы канала.
; Правится из игры: F7 -> вкладка Twitch. Руками тоже можно.
;
; reward = id|вкл|тип|клавиша|мс|цена|id_награды|название|подпись|задержка|метка|перезарядка
;   метка       - что последний раз уехало на Twitch, служебное поле
;   тип         - hold (держать), press (нажать), spawn (враг) или effect (эффект)
;   клавиша     - для spawn/effect это ключ из списка в F7 -> Награды
;   мс          - сколько держать (hold), живёт враг (spawn) или длится (effect)
;   задержка    - у spawn через сколько секунд появится враг,
';                 у effect 1 = не исполнять в бою с боссом
;   цена        - справочно, баллы списывает сам Twitch
;   название    - как награда называется на дашборде Twitch
;   перезарядка - только у effect: сколько секунд после покупки эту же
;                 награду нельзя купить снова (0 = без перезарядки)
;
; Сами награды создаёт стример на дашборде Twitch - мод их только слушает.

";

pub fn save(hmodule: usize, entries: &[RewardEntry]) {
    let Some(path) = dll_sibling(hmodule, REWARDS_FILE) else {
        return;
    };
    let mut text = String::from(HEADER);
    for e in entries {
        text.push_str(&encode(e));
        text.push('\n');
    }
    crate::config::write_atomic(&path, &text);
}

pub fn next_id(entries: &[RewardEntry]) -> u32 {
    entries.iter().map(|e| e.id).max().unwrap_or(0) + 1
}

/// Какая запись отвечает за эту покупку.
///
/// По `reward_id`, если он задан, иначе по названию без учёта регистра -
/// это основной путь: стример копирует название с дашборда, а не ищет UUID.
pub fn find_match<'a>(entries: &'a [RewardEntry], reward_id: &str, title: &str) -> Option<&'a RewardEntry> {
    entries.iter().find(|e| {
        if !e.reward_id.is_empty() {
            return e.reward_id == reward_id;
        }
        // `to_lowercase`, а не `eq_ignore_ascii_case`: названия наград чаще
        // всего русские, а ASCII-версия кириллицу не складывает по регистру
        // вовсе - «Прыжок» и «прыжок» считались бы разными наградами.
        !e.reward_title.is_empty() && e.reward_title.trim().to_lowercase() == title.trim().to_lowercase()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Жалоба 2026-08-19: «h, m и вся вторая половина клавиатуры не биндятся».
    /// Причина была в двух списках: `vk_of` знал все буквы, а назначить можно
    /// было только те, что попали во второй.
    #[test]
    fn the_whole_keyboard_is_bindable() {
        for key in [Key::H, Key::M, Key::I, Key::J, Key::K, Key::L, Key::N, Key::O, Key::P, Key::U, Key::Y] {
            assert!(vk_of(key).is_some(), "{key:?} не назначается");
        }
        for key in [Key::Alpha1, Key::F5, Key::Keypad7, Key::Comma, Key::Backspace] {
            assert!(vk_of(key).is_some(), "{key:?} не назначается");
        }
    }

    /// Одна клавиша - одна строка. Дубль означал бы, что `vk_of` молча берёт
    /// первую, а вторая строка в списке ни на что не влияет.
    #[test]
    fn the_key_table_has_no_duplicates() {
        for (i, (key, vk)) in KEYS.iter().enumerate() {
            for (other, other_vk) in &KEYS[i + 1..] {
                assert_ne!(key, other, "клавиша {key:?} в таблице дважды");
                assert_ne!(vk, other_vk, "код {vk:#04X} у двух клавиш");
            }
        }
    }

    /// Жалоба 2026-08-19: «по кнопке проверка не нажимаются стрелочки».
    /// Скан-код стрелки совпадает со скан-кодом цифрового блока, и без флага
    /// расширенной клавиши игра получала Num 8 вместо стрелки вверх.
    #[test]
    fn arrows_and_the_nav_block_are_extended() {
        for key in [
            Key::UpArrow, Key::DownArrow, Key::LeftArrow, Key::RightArrow,
            Key::Insert, Key::Delete, Key::Home, Key::End, Key::PageUp, Key::PageDown,
            Key::RightCtrl, Key::RightAlt,
        ] {
            let vk = vk_of(key).expect("клавиша есть в таблице");
            assert!(EXTENDED_VKS.contains(&vk), "{key:?} обязана слаться расширенной");
        }
        // А обычные - наоборот: лишний 0xE0 увёл бы их в другую клавишу.
        for key in [Key::W, Key::Space, Key::LeftCtrl, Key::Keypad8, Key::Enter] {
            let vk = vk_of(key).expect("клавиша есть в таблице");
            assert!(!EXTENDED_VKS.contains(&vk), "{key:?} расширенной быть не должна");
        }
    }

    /// Подпись клавиши не должна быть отладочным `{:?}`: «Alpha1» и
    /// «GraveAccent» в списке наград читаются как мусор.
    #[test]
    fn labels_are_human_readable() {
        assert_eq!(key_label(Key::Alpha1), "1");
        assert_eq!(key_label(Key::Keypad7), "Num 7");
        assert_eq!(key_label(Key::GraveAccent), "`");
        assert_eq!(key_label(Key::W), "W");
        assert_eq!(key_label(Key::F5), "F5");
        // Ни одна подпись не должна остаться сырым именем варианта.
        for (key, _) in KEYS {
            let label = key_label(*key);
            assert!(!label.starts_with("Alpha") && !label.starts_with("Keypad"), "{key:?} -> {label}");
        }
    }

    /// Код в списке расширенных, которого нет в таблице клавиш, - опечатка:
    /// он не сработает никогда и не будет замечен.
    #[test]
    fn extended_list_has_no_orphans() {
        for vk in EXTENDED_VKS {
            assert!(KEYS.iter().any(|(_, k)| k == vk), "код {vk:#04X} ни к какой клавише не привязан");
        }
    }

    /// Метка синхронизации отвечает на один вопрос: то же самое сейчас на
    /// Twitch, что и здесь, или уже нет. Без неё правка цены молча не
    /// доезжала до дашборда (жалоба 2026-08-23).
    #[test]
    fn sync_mark_notices_a_changed_price_or_title() {
        let mut e = hold();
        assert_eq!(e.in_sync(), None, "награды на Twitch ещё нет");

        e.reward_id = "uuid-1".into();
        e.synced = e.mark();
        assert_eq!(e.in_sync(), Some(true));

        e.cost += 50;
        assert_eq!(e.in_sync(), Some(false), "цену поменяли - надо обновлять");

        e.cost -= 50;
        assert_eq!(e.in_sync(), Some(true), "вернули как было - обновлять нечего");

        e.reward_title = "Другое имя".into();
        assert_eq!(e.in_sync(), Some(false), "название тоже уезжает на Twitch");

        // Пробелы по краям на Twitch всё равно не уедут - `create_reward` их
        // срезает, и метка обязана вести себя так же.
        let trimmed = fingerprint("Прыжок", 100);
        assert_eq!(fingerprint("  Прыжок  ", 100), trimmed);

        // А вот галочка мода на Twitch не уезжает вовсе, и «изменено» от неё
        // загораться не должно: она про то, исполняет ли мод покупку.
        e.reward_title = "Шаг вперёд".into();
        e.enabled = !e.enabled;
        assert_eq!(e.in_sync(), Some(true), "галочка мода - не изменение для Twitch");
    }

    /// Метка переживает запись в файл: иначе после перезапуска игры каждая
    /// награда показывалась бы как «изменено».
    #[test]
    fn sync_mark_survives_a_round_trip() {
        let mut e = hold();
        e.reward_id = "uuid-1".into();
        e.synced = e.mark();
        let back = decode(encode(&e).split_once('=').expect("строка вида reward = ...").1)
            .expect("строка разбирается");
        assert_eq!(back.synced, e.synced);
        assert_eq!(back.in_sync(), Some(true));
    }

    fn hold() -> RewardEntry {
        RewardEntry {
            id: 1,
            enabled: true,
            action: Action::Hold { key: Key::W, duration_ms: 500 },
            cost: 100,
            reward_id: String::new(),
            reward_title: "Шаг вперёд".into(),
            label: "идём".into(),
            synced: 0,
        }
    }

    fn round_trip(e: &RewardEntry) -> RewardEntry {
        decode(encode(e).split_once('=').unwrap().1).expect("строка должна читаться обратно")
    }

    #[test]
    fn round_trips() {
        let e = hold();
        assert_eq!(round_trip(&e), e);

        let p = RewardEntry { action: Action::Press { key: Key::Space }, label: String::new(), ..hold() };
        assert_eq!(round_trip(&p), p);
    }

    /// Спавн - третий тип действия, и у него другое поле "клавиша" (ключ
    /// куратор-списка, не реальная клавиша). Обязан пережить round-trip так
    /// же, как press/hold.
    /// Клавиатура нужна только нажатию и удержанию. Спавн и эффект пишут
    /// игровую память, поэтому идут и при открытом окне настроек, и когда игра
    /// не активное окно (запрос 2026-09-08).
    #[test]
    fn only_key_actions_need_the_keyboard() {
        let key = crate::spawn::SPAWN_TABLE[0].key;
        assert!(Action::Press { key: Key::Space }.needs_keyboard());
        assert!(Action::Hold { key: Key::W, duration_ms: 500 }.needs_keyboard());
        assert!(
            !Action::SpawnEnemy { key, delay_secs: 0, cooldown_secs: 0, ally: false, ttl_secs: 0 }
                .needs_keyboard()
        );
        assert!(!Action::Effect {
            key: crate::effects::EFFECT_TABLE[0].key,
            secs: 10,
            in_boss: true,
            cooldown_secs: 0
        }
        .needs_keyboard());
    }

    #[test]
    fn spawn_action_round_trips() {
        let key = crate::spawn::SPAWN_TABLE[0].key;
        let s = RewardEntry { action: Action::SpawnEnemy { key, delay_secs: 0, cooldown_secs: 0, ally: false, ttl_secs: 0 }, ..hold() };
        assert_eq!(round_trip(&s), s);

        // Задержка и перезарядка тоже обязаны пережить круг: обе едут в
        // хвостовых полях строки.
        let timed = RewardEntry {
            action: Action::SpawnEnemy { key, delay_secs: 10, cooldown_secs: 45, ally: true, ttl_secs: 90 },
            ..hold()
        };
        assert_eq!(round_trip(&timed), timed);
    }

    /// Эффект - четвёртый тип действия, и у него в девятом поле не задержка, а
    /// запрет в бою с боссом.
    ///
    /// **Хранится именно ЗАПРЕТ, а не разрешение**, и это не придирка: ноль в
    /// строках, написанных до появления галочки, обязан читаться как
    /// «разрешено», то есть как мод вёл себя раньше.
    #[test]
    fn effect_action_round_trips_with_its_boss_flag() {
        let key = crate::effects::EFFECT_TABLE[0].key;
        for in_boss in [true, false] {
            let e = RewardEntry {
                action: Action::Effect { key, secs: 90, in_boss, cooldown_secs: 30 },
                ..hold()
            };
            assert_eq!(round_trip(&e), e, "in_boss = {in_boss}");
        }

        // Строка прошлой версии мода: девятого поля нет вовсе, двенадцатого
        // (перезарядка) - тем более.
        let old = decode(&format!("7|1|effect|{key}|0|100")).expect("старая строка читается");
        assert!(old.action.allowed_in_boss(), "без поля награда обязана работать везде");
        assert_eq!(old.action.cooldown_secs(), 0, "без поля перезарядки быть не должно");
    }

    /// Ключ, которого нет в куратор-списке (стёрли строку, опечатка руками) -
    /// строка не грузится, а не подставляет случайного врага.
    #[test]
    fn spawn_kind_with_unknown_key_is_skipped() {
        assert!(decode("5|1|spawn|нет-такого-ключа|0|0").is_none());
    }

    /// `|` в названии награды и в подписи не должен разваливать строку: их
    /// пишет человек, и вертикальная черта там вполне возможна.
    #[test]
    fn pipe_in_free_text_survives() {
        let e = RewardEntry { reward_title: "прыжок | jump".into(), label: "a|b".into(), ..hold() };
        let back = round_trip(&e);
        assert!(!back.reward_title.contains('|'), "{}", back.reward_title);
        assert!(back.reward_title.starts_with("прыжок"));
        assert_eq!(back.action, e.action, "действие не должно съехать на соседнее поле");
        assert_eq!(back.cost, e.cost);
    }

    /// Старая строка без хвостовых полей читается, а не теряется.
    #[test]
    fn short_line_still_loads() {
        let e = decode("7|1|press|Space|0|50").unwrap();
        assert_eq!(e.id, 7);
        assert_eq!(e.action, Action::Press { key: Key::Space });
        assert_eq!(e.cost, 50);
        assert!(e.reward_title.is_empty());
    }

    /// Строка спавна, написанная до появления тринадцатого поля, обязана
    /// читаться ВРАГОМ. Ноль в нём - это «как было раньше», и молчаливое
    /// превращение чужих наград в союзников было бы худшим из исходов.
    #[test]
    fn a_spawn_line_without_the_ally_field_stays_hostile() {
        let key = crate::spawn::SPAWN_TABLE[0].key;
        // Пятое поле - 90: у строк той версии там лежало время жизни, и
        // теперь оно снова значит ровно это.
        let line = format!("8|1|spawn|{key}|90000|100||||3|0|60");
        let e = decode(&line).expect("строка прошлой версии грузится");
        assert_eq!(
            e.action,
            Action::SpawnEnemy { key, delay_secs: 3, cooldown_secs: 60, ally: false, ttl_secs: 90 }
        );
    }

    #[test]
    fn broken_lines_are_skipped_not_fatal() {
        let text = "; комментарий\n\
                    reward = 1|1|hold|W|500|100||Шаг|\n\
                    reward = мусор\n\
                    reward = 2|1|нетакого|W|0|0\n\
                    reward = 3|1|press|НетТакойКлавиши|0|0\n\
                    что-то совсем не то\n\
                    reward = 4|0|press|Space|0|10||Прыжок|\n";
        let all = parse_all(text);
        assert_eq!(all.len(), 2, "две валидные строки из шести");
        assert_eq!(all[0].id, 1);
        assert_eq!(all[1].id, 4);
        assert!(!all[1].enabled, "выключенная запись всё равно грузится");
    }

    #[test]
    fn matches_by_title_then_by_id() {
        let by_title = hold();
        let by_id = RewardEntry { id: 2, reward_id: "uuid-9".into(), reward_title: "другое".into(), ..hold() };
        let all = vec![by_title, by_id];

        // Название - без учёта регистра и лишних пробелов.
        assert_eq!(find_match(&all, "", " шаг ВПЕРЁД ").map(|e| e.id), Some(1));
        // Заполненный uuid важнее названия.
        assert_eq!(find_match(&all, "uuid-9", "совсем не то").map(|e| e.id), Some(2));
        assert_eq!(find_match(&all, "", "нет такой награды"), None);
    }

    /// Названия наград почти всегда русские, а `eq_ignore_ascii_case`
    /// кириллицу по регистру не складывает - на этом тест и поймал ошибку.
    #[test]
    fn cyrillic_titles_ignore_case() {
        let e = RewardEntry { reward_title: "Прыжок".into(), ..hold() };
        let all = [e];
        assert!(find_match(&all, "", "прыжок").is_some());
        assert!(find_match(&all, "", "ПРЫЖОК").is_some());
        assert!(find_match(&all, "", " Прыжок ").is_some());
        assert!(find_match(&all, "", "прыжки").is_none());
    }

    /// Запись с пустым названием не должна ловить любую покупку подряд.
    #[test]
    fn empty_title_matches_nothing() {
        let blank = RewardEntry { reward_title: String::new(), ..hold() };
        assert_eq!(find_match(&[blank], "", ""), None);
    }

    #[test]
    fn caption_falls_back_to_reward_title() {
        assert_eq!(hold().caption(), "идём");
        assert_eq!(RewardEntry { label: "  ".into(), ..hold() }.caption(), "Шаг вперёд");
    }

    #[test]
    fn ids_keep_growing() {
        assert_eq!(next_id(&[]), 1);
        assert_eq!(next_id(&[RewardEntry { id: 5, ..hold() }]), 6);
    }
}
