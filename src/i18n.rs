//! Язык интерфейса.
//!
//! В коде остаётся только английский текст, и он же служит ключом:
//! `t("Deaths")`. Переводы лежат внешними файлами `locale/<код>.ini` рядом с
//! DLL - по файлу на язык, имя файла и есть код языка. Новый файл в папке сам
//! становится ещё одной радиокнопкой в настройках, пересобирать мод не нужно.
//!
//! Перевода нет - на экране английский из кода. Сломать интерфейс кривым или
//! неполным файлом нельзя.
//!
//! Строки разбираются ОДИН раз при загрузке и уезжают в `Box::leak`: и панель
//! (`Grid::card` берёт `title: &'static str`), и hudhook требуют `'static`, а
//! переключение языка после этого стоит одного `store` в атомик.
//!
//! По умолчанию язык берётся из самой игры - через Steam. У игр FromSoftware
//! нет своего меню языка: то, на каком языке говорит Elden Ring, решает
//! настройка "язык игры" в Steam, и её же читаем мы.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use hudhook::windows::core::PCSTR;
use hudhook::windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

/// Значение `lang` в `.ini`, означающее "как в игре".
pub const AUTO: &str = "auto";

/// Наши переводы едут внутри DLL и выкладываются на диск, если файла там нет.
/// Свежая установка - это одна DLL, и без этого мод в ней говорил бы только
/// по-английски, а скопировать образец для своего языка было бы неоткуда.
const BUILT_IN: &[(&str, &str)] = &[
    ("en.ini", include_str!("../locale/en.ini")),
    ("ru.ini", include_str!("../locale/ru.ini")),
];

/// Латиница и кириллица нужны всегда, независимо от локали: английский текст
/// зашит в коде, а кириллица приезжает из самой игры (имена боссов на русской
/// сборке). Всё остальное добавляет уже перевод.
const BASE_RANGES: &[u32] = &[0x20, 0xFF, 0x0400, 0x052F, 0];

pub struct Language {
    /// Имя файла без расширения: `ru`, `en`, `pt-br`.
    pub code: &'static str,
    /// Как язык подписан в настройках (`_name` в файле).
    pub name: &'static str,
    /// Что возвращает Steam для этого языка (`_steam`), нижним регистром.
    steam: Vec<&'static str>,
    map: HashMap<&'static str, &'static str>,
    ranges: &'static [u32],
}

impl Language {
    fn fits_game(&self, game: &str) -> bool {
        self.steam.contains(&game) || (!game.is_empty() && game.starts_with(self.code))
    }
}

static LOCALES: OnceLock<Vec<Language>> = OnceLock::new();

/// Индекс в `LOCALES`. `NONE` - перевода нет, показываем английский из кода.
const NONE: usize = usize::MAX;
static ACTIVE: AtomicUsize = AtomicUsize::new(NONE);

/// Прочитать папку `locale` рядом с DLL. Зовётся один раз, из `Config::load`
/// перед `apply`.
pub fn load(dll_hmodule: usize) {
    LOCALES.get_or_init(|| {
        crate::config::dll_sibling(dll_hmodule, "locale").map(|d| read_dir(&d)).unwrap_or_default()
    });
}

/// Что предложить в настройках. Пусто - значит папки нет вовсе, и выбирать
/// не из чего: интерфейс английский.
pub fn languages() -> &'static [Language] {
    LOCALES.get().map(Vec::as_slice).unwrap_or(&[])
}

/// Разворачивает выбор в конкретный файл. `AUTO` - спросить Steam.
pub fn apply(code: &str) {
    let code = code.trim();
    let found = if code.eq_ignore_ascii_case(AUTO) {
        let game = game_language();
        languages().iter().position(|l| l.fits_game(&game))
    } else {
        languages().iter().position(|l| l.code.eq_ignore_ascii_case(code))
    };
    ACTIVE.store(found.unwrap_or(NONE), Ordering::Relaxed);
}

/// Код активного языка. Уезжает в JSON виджету OBS и в `font_signature`.
pub fn active() -> &'static str {
    current().map_or("en", |l| l.code)
}

/// Перевод английской строки. Хвост `##...` - уточнение ключа для случаев,
/// где одна английская строка значит на другом языке разное; на экран он не
/// идёт никогда.
pub fn t(en: &'static str) -> &'static str {
    if let Some(l) = current() {
        if let Some(s) = l.map.get(en) {
            return s;
        }
    }
    plain(en)
}

/// Диапазоны глифов для атласа: базовые плюс всё, что есть в переводе.
pub fn glyph_ranges() -> &'static [u32] {
    current().map_or(BASE_RANGES, |l| l.ranges)
}

fn current() -> Option<&'static Language> {
    let i = ACTIVE.load(Ordering::Relaxed);
    if i == NONE {
        return None;
    }
    LOCALES.get()?.get(i)
}

/// Ключ без хвоста-уточнения - ровно то, что видит зритель.
fn plain(en: &str) -> &str {
    match en.find("##") {
        Some(i) => &en[..i],
        None => en,
    }
}

// ---------------------------------------------------------------------------
// Файлы
// ---------------------------------------------------------------------------

fn read_dir(dir: &Path) -> Vec<Language> {
    let _ = std::fs::create_dir_all(dir);
    for (name, text) in BUILT_IN {
        sync_built_in(&dir.join(name), text);
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
        if ext.as_deref() != Some("ini") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).map(str::to_string);
        let (Some(code), Ok(text)) = (stem, std::fs::read_to_string(&path)) else {
            continue;
        };
        out.push(build(&code, &text));
    }
    // Порядок каталога зависит от файловой системы, а список языков в
    // настройках прыгать не должен.
    out.sort_by(|a, b| a.code.cmp(b.code));
    out
}

/// Файла нет - кладём свой. Файл есть - дописываем ключи, которых в нём не
/// хватает: иначе после обновления мода новые подписи молча ушли бы в
/// английский, а правки пользователя терять нельзя.
fn sync_built_in(path: &Path, built_in: &str) {
    let Ok(disk) = std::fs::read_to_string(path) else {
        crate::config::write_atomic(path, built_in);
        return;
    };
    let have = parse(&disk);
    let mut add = String::new();
    for (key, value) in parse(built_in).pairs {
        if !have.pairs.iter().any(|(k, _)| *k == key) {
            add.push_str(&format!("{key} = {value}\n"));
        }
    }
    if !add.is_empty() {
        crate::config::write_atomic(path, &format!("{disk}\n; --- added by a mod update ---\n{add}"));
    }
}

fn build(code: &str, text: &str) -> Language {
    let parsed = parse(text);
    let name = parsed.name.unwrap_or_else(|| code.to_string());
    let ranges: &'static [u32] = Box::leak(ranges_of(&parsed.pairs, &name).into_boxed_slice());
    Language {
        code: Box::leak(code.to_string().into_boxed_str()),
        name: Box::leak(name.into_boxed_str()),
        steam: parsed.steam.into_iter().map(|s| &*Box::leak(s.into_boxed_str())).collect(),
        map: parsed
            .pairs
            .into_iter()
            .map(|(k, v)| (&*Box::leak(k.into_boxed_str()), &*Box::leak(v.into_boxed_str())))
            .collect(),
        ranges,
    }
}

#[derive(Default)]
struct Parsed {
    name: Option<String>,
    steam: Vec<String>,
    pairs: Vec<(String, String)>,
}

/// Построчный разбор. Делим по ПЕРВОМУ `=`: ключ - английская строка, и знака
/// равенства в ней быть не должно (тест это держит), а вот в переводе он
/// встречается свободно.
fn parse(text: &str) -> Parsed {
    let mut p = Parsed::default();
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() || value.is_empty() {
            continue;
        }
        match key {
            "_name" => p.name = Some(value.to_string()),
            "_steam" => {
                p.steam = value.split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect();
            }
            _ => p.pairs.push((key.to_string(), value.to_string())),
        }
    }
    p
}

/// Пары `начало, конец` плюс нулевой терминатор - в том виде, в каком их ждёт
/// `FontGlyphRanges::from_slice`.
///
/// Она паникует на пересечении, на неотсортированном входе и на нулевом
/// глифе, а в релизе `panic = "abort"` - то есть краш игры. Поэтому пары
/// строятся из отсортированного множества и сливаются, а не собираются на
/// глаз.
fn ranges_of(pairs: &[(String, String)], name: &str) -> Vec<u32> {
    let mut set: Vec<u32> = BASE_RANGES.chunks(2).filter(|c| c.len() == 2).flat_map(|c| c[0]..=c[1]).collect();
    for c in pairs.iter().flat_map(|(_, v)| v.chars()).chain(name.chars()) {
        let u = c as u32;
        if u >= 0x20 {
            set.push(u);
        }
    }
    set.sort_unstable();
    set.dedup();

    let mut out: Vec<u32> = Vec::new();
    for u in set {
        match out.last_mut() {
            Some(end) if u <= *end + 1 => *end = u,
            _ => {
                out.push(u);
                out.push(u);
            }
        }
    }
    out.push(0);
    out
}

// ---------------------------------------------------------------------------
// Язык игры
// ---------------------------------------------------------------------------

/// Кэшируется навсегда: сменить язык игры без её перезапуска всё равно нельзя,
/// а лезть в чужие vtable на каждое переключение незачем.
fn game_language() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(|| steam_language().unwrap_or_default().to_ascii_lowercase()).clone()
}

/// `ISteamApps::GetCurrentGameLanguage` - код языка, на котором игра
/// показывает свой текст ("english", "russian", ...). Не язык клиента Steam и
/// не локаль системы.
///
/// `steam_api64.dll` игра уже загрузила, поэтому интерфейс достаётся через
/// `GetProcAddress`, а не линковкой закрытого SDK. Слот 4 в vtable и строки
/// версий - из публичных заголовков Steamworks (в elden это же место расписано
/// подробнее). Неподходящая строка версии возвращает null, а не мусорный
/// указатель, поэтому перебрать обе безопасно.
fn steam_language() -> Option<String> {
    const GET_CURRENT_GAME_LANGUAGE_SLOT: usize = 4;
    unsafe {
        let module = GetModuleHandleA(PCSTR(c"steam_api64.dll".as_ptr().cast())).ok()?;
        let get_h_user = GetProcAddress(module, PCSTR(c"SteamAPI_GetHSteamUser".as_ptr().cast()))?;
        let find = GetProcAddress(module, PCSTR(c"SteamInternal_FindOrCreateUserInterface".as_ptr().cast()))?;

        let get_h_user: unsafe extern "system" fn() -> i32 = std::mem::transmute(get_h_user);
        let find: unsafe extern "system" fn(i32, *const u8) -> *mut c_void = std::mem::transmute(find);

        let h_user = get_h_user();
        let iface = [c"STEAMAPPS_INTERFACE_VERSION008", c"STEAMAPPS_INTERFACE_VERSION009"]
            .into_iter()
            .map(|v| find(h_user, v.as_ptr().cast()))
            .find(|p| !p.is_null())?;

        let vtable = *(iface as *const *const *const c_void);
        let f: unsafe extern "system" fn(*mut c_void) -> *const c_char =
            std::mem::transmute(*vtable.add(GET_CURRENT_GAME_LANGUAGE_SLOT));
        let ptr = f(iface);
        if ptr.is_null() {
            return None;
        }
        CStr::from_ptr(ptr).to_str().ok().map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Разбор: комментарии, пустые строки, BOM, `=` в переводе, хвост `##`.
    #[test]
    fn locale_file_round_trips() {
        let p = parse(concat!(
            "\u{feff}; comment\n",
            "[section]\n",
            "\n",
            "_name = Русский\n",
            "_steam = russian, ru_RU\n",
            "Deaths = Смерти\n",
            "Deaths##fight = Смертей\n",
            "Hold for, s (0 - a tap) = Держать, с (0 = обычное нажатие)\n",
            "broken line without a separator\n",
            "= value without a key\n",
            "Empty =\n",
        ));
        assert_eq!(p.name.as_deref(), Some("Русский"));
        assert_eq!(p.steam, vec!["russian", "ru_ru"]);
        assert_eq!(p.pairs.len(), 3, "{:?}", p.pairs);
        assert_eq!(p.pairs[0], ("Deaths".into(), "Смерти".into()));
        assert_eq!(p.pairs[1], ("Deaths##fight".into(), "Смертей".into()));
        // Знак равенства в переводе не режет строку: делим по первому.
        assert_eq!(p.pairs[2].1, "Держать, с (0 = обычное нажатие)");
    }

    /// Хвост-уточнение никогда не доезжает до экрана.
    #[test]
    fn the_hash_suffix_never_reaches_the_screen() {
        assert_eq!(plain("Deaths##fight"), "Deaths");
        assert_eq!(plain("Deaths"), "Deaths");
        // Без загруженной локали `t` отдаёт сам ключ, обрезанный по хвосту.
        assert_eq!(t("Deaths##fight"), "Deaths");
    }

    /// `FontGlyphRanges::from_slice` паникует на кривом диапазоне, а в релизе
    /// `panic = "abort"` - это краш игры. Проверяем ровно то, что она требует.
    #[test]
    fn glyph_ranges_are_valid_for_imgui() {
        let pairs = vec![
            ("a".to_string(), "Zażółć gęślą jaźń".to_string()),
            ("b".to_string(), "日本語 тире".to_string()),
        ];
        let r = ranges_of(&pairs, "Polski");
        assert_eq!(r.len() % 2, 1, "длина обязана быть нечётной");
        assert_eq!(r.last(), Some(&0), "нужен нулевой терминатор");
        let body = &r[..r.len() - 1];
        assert!(body.iter().all(|&g| g != 0 && g <= char::MAX as u32));
        let pairs: Vec<&[u32]> = body.chunks(2).collect();
        for p in &pairs {
            assert!(p[0] <= p[1], "начало больше конца: {p:?}");
        }
        // Пары идут по возрастанию и не соприкасаются - иначе ImGui паникует.
        for w in pairs.windows(2) {
            assert!(w[0][1] + 1 < w[1][0], "диапазоны слиплись: {:?} и {:?}", w[0], w[1]);
        }
        let covers = |c: u32| pairs.iter().any(|p| p[0] <= c && c <= p[1]);
        // Базовое всегда внутри, что бы ни лежало в переводе.
        assert!(covers(0x41), "латиница пропала");
        assert!(covers(0x0416), "кириллица пропала");
        assert!(covers(0x017C), "ż из перевода пропала");
        assert!(covers(0x65E5), "иероглиф из перевода пропал");
    }

    /// Собранный из файла язык действительно переводит и действительно
    /// подписан: без этого «локаль читается» и «локаль работает» - разные
    /// вещи, и разницу видно только на экране.
    #[test]
    fn a_built_language_translates_and_names_itself() {
        let ru = build("ru", BUILT_IN[1].1);
        assert_eq!(ru.code, "ru");
        assert_eq!(ru.name, "\u{420}\u{443}\u{441}\u{441}\u{43a}\u{438}\u{439}");
        assert!(ru.fits_game("russian"), "auto должен выбирать его на русской игре");
        assert!(!ru.fits_game("english"));
        assert_eq!(ru.map.get("Language").copied(), Some("\u{42f}\u{437}\u{44b}\u{43a}"));
        // Хвост-уточнение переводится отдельно от базовой строки.
        assert_ne!(ru.map.get("Deaths").copied(), ru.map.get("Deaths##fight").copied());
        // Кириллица из перевода попала в диапазон глифов.
        assert!(ru.ranges.chunks(2).any(|p| p.len() == 2 && p[0] <= 0x42f && 0x42f <= p[1]));
    }

    /// Пустая папка - мод кладёт туда свои файлы и читает их обратно;
    /// чужой файл рядом становится ещё одним языком.
    ///
    /// Единственный тест, который трогает диск, и делает это во временной
    /// папке, а не рядом с DLL.
    #[test]
    fn an_empty_folder_fills_itself_and_picks_up_a_stranger() {
        let dir = std::env::temp_dir().join("ios_locale_test");
        let _ = std::fs::remove_dir_all(&dir);

        let langs = read_dir(&dir);
        assert_eq!(langs.len(), 2, "должны появиться ru и en");
        assert!(dir.join("ru.ini").exists() && dir.join("en.ini").exists());
        assert_eq!(langs.iter().map(|l| l.code).collect::<Vec<_>>(), ["en", "ru"]);

        // Чужой файл: код из имени, подпись из `_name`, перевод работает.
        std::fs::write(dir.join("pl.ini"), "_name = Polski\n_steam = polish\nLevel = Poziom\n").unwrap();
        // Не .ini рядом не мешает.
        std::fs::write(dir.join("readme.txt"), "not a locale").unwrap();
        let langs = read_dir(&dir);
        let pl = langs.iter().find(|l| l.code == "pl").expect("польский не нашёлся");
        assert_eq!(pl.name, "Polski");
        assert_eq!(pl.map.get("Level").copied(), Some("Poziom"));
        // Непереведённое остаётся английским, а не пропадает.
        assert_eq!(pl.map.get("Deaths"), None);
        assert_eq!(langs.len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Все файлы, откуда текст доезжает до экрана: и рисующие модули, и те,
    /// что собирают строки в рантайме (статус Twitch, причины отказа).
    /// `i18n.rs` в списке нет намеренно - у него в шапке живут примеры вида
    /// `t("Deaths")`, которые ключами не являются.
    const SOURCES: &[(&str, &str)] = &[
        ("settings.rs", include_str!("settings.rs")),
        ("overlay.rs", include_str!("overlay.rs")),
        ("lib.rs", include_str!("lib.rs")),
        ("spawn.rs", include_str!("spawn.rs")),
        ("web.rs", include_str!("web.rs")),
        ("effects.rs", include_str!("effects.rs")),
        ("twitch/mod.rs", include_str!("twitch/mod.rs")),
        ("twitch/auth.rs", include_str!("twitch/auth.rs")),
        ("twitch/eventsub.rs", include_str!("twitch/eventsub.rs")),
        ("twitch/rewards.rs", include_str!("twitch/rewards.rs")),
    ];

    /// Литералы из вызовов `t("...")`. Разбирать Rust целиком незачем: нас
    /// интересует ровно одна форма, а всё, что на неё не похоже (`insert(`,
    /// `expect(`, `assert(`), отсекается проверкой символа перед `t`.
    ///
    /// Экранирование снимается: ключом в файле служит та строка, которую
    /// увидит `t()` в рантайме, а не то, как она записана в коде.
    fn keys_in(src: &str) -> Vec<String> {
        let bytes = src.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(p) = src[i..].find("t(") {
            let at = i + p;
            i = at + 2;
            if at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_') {
                continue;
            }
            let rest = src[i..].trim_start();
            let Some(body) = rest.strip_prefix('"') else { continue };
            let b = body.as_bytes();
            let mut end = 0;
            while end < b.len() {
                match b[end] {
                    b'\\' => end += 2,
                    b'"' => break,
                    _ => end += 1,
                }
            }
            if end >= b.len() {
                continue;
            }
            // Дальше обязана быть закрывающая скобка - иначе это не наш вызов.
            if body[end + 1..].trim_start().starts_with(')') {
                out.push(unescape(&body[..end]));
            }
        }
        out
    }

    fn unescape(lit: &str) -> String {
        let mut out = String::with_capacity(lit.len());
        let mut it = lit.chars();
        while let Some(c) = it.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => out.push(other),
                None => {}
            }
        }
        out
    }

    /// Всё, что должно быть в файлах локали: строки из кода плюс подписи
    /// куратор-списков. Таблицы берём напрямую - они `const`, парсить их
    /// исходник было бы вторым разбором того же.
    fn every_key() -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (_, src) in SOURCES {
            out.extend(keys_in(src));
        }
        for (_, full, short) in crate::web::WEB_LABELS {
            out.push(full.to_string());
            out.push(short.to_string());
        }
        out.extend(crate::spawn::SPAWN_TABLE.iter().map(|e| e.label.to_string()));
        out.extend(crate::spawn::RANDOM_PICKS.iter().map(|p| p.label.to_string()));
        for e in crate::effects::EFFECT_TABLE {
            out.push(e.label.to_string());
            if !e.about.is_empty() {
                out.push(e.about.to_string());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Единственное, что не даёт переводу подгнить: добавил строку в код -
    /// тест называет её и говорит, в какой файл дописать.
    ///
    /// Заодно ловит обратное - ключ, оставшийся в файле от удалённой подписи.
    #[test]
    fn every_source_string_is_in_the_locale_files() {
        let want = every_key();
        for (name, text) in BUILT_IN {
            let have: Vec<String> = parse(text).pairs.into_iter().map(|(k, _)| k).collect();
            let missing: Vec<&String> = want.iter().filter(|k| !have.contains(k)).collect();
            let stale: Vec<&String> = have.iter().filter(|h| !want.contains(h)).collect();
            assert!(missing.is_empty(), "locale/{name}: нет перевода для {missing:#?}");
            assert!(stale.is_empty(), "locale/{name}: лишние ключи от удалённых подписей {stale:#?}");
        }
    }

    /// Ключ - это то, что видит переводчик слева от `=`. Знак равенства в нём
    /// разрезал бы строку файла, а пробелы по краям молча съедаются `trim`.
    #[test]
    fn source_strings_are_usable_as_keys() {
        for k in &every_key() {
            assert!(!k.contains('='), "знак равенства в ключе: {k:?}");
            assert_eq!(k, k.trim(), "пробел по краям ключа: {k:?}");
            assert!(!k.is_empty(), "пустой ключ");
            assert!(!k.starts_with(';') && !k.starts_with('['), "ключ выглядит комментарием: {k:?}");
            assert!(!k.contains('\n'), "перевод строки в ключе: {k:?}");
        }
    }

    /// В английском файле перевод обязан совпадать с ключом: он и есть
    /// исходный текст, а заодно образец, с которого снимают свой язык.
    #[test]
    fn the_english_file_is_the_source_itself() {
        for (key, value) in parse(BUILT_IN[0].1).pairs {
            assert_eq!(plain(&key), value, "en.ini разошёлся с кодом: {key:?}");
        }
    }
}
