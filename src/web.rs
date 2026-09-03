//! Крошечный HTTP на 127.0.0.1 для OBS Browser Source.
//!
//! `std::net`, без зависимостей: два фиксированных маршрута, никакого keep-alive
//! и никакой отдачи файлов по пути из запроса (а значит и path traversal
//! неоткуда взяться). Слушаем только петлю - наружу порт не торчит.
//!
//! ponytail: поллинг вместо WebSocket. Цифры меняются раз в секунды, а fetch
//! раз в 250 мс - это вдвое меньше кода, чем рукописный WS-хендшейк. Переходить
//! на WS, если понадобится мгновенная реакция (алерты в чат).

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use crate::config::{color_to_hex, BossCount, Config};
use crate::stats::Snapshot;

const PAGE: &str = include_str!("../web/index.html");

/// То, что отдаётся странице. Конфиг тут же, чтобы страница не рисовала
/// выключенное - галочки живут в одном месте, в `.ini`.
pub struct Shared {
    pub snapshot: Snapshot,
    pub config: Config,
    pub twitch: TwitchWeb,
}

/// Состояние Twitch для страницы. Отдельно от `Snapshot`: тот про игровую
/// память, а это про сеть, и собираются они разными потоками.
#[derive(Clone, Default)]
pub struct TwitchWeb {
    pub connected: bool,
    /// Последние покупки, новые в конце.
    pub recent: Vec<crate::twitch::PurchaseEvent>,
}

pub fn spawn(shared: Arc<Mutex<Shared>>, port: u16) {
    std::thread::spawn(move || {
        // Порт занят (вторая копия игры, чужая программа) - живём без
        // веб-выхода, оверлей от этого не страдает.
        let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) else {
            return;
        };
        for stream in listener.incoming().flatten() {
            let _ = serve(stream, &shared);
        }
    });
}

fn serve(mut stream: TcpStream, shared: &Mutex<Shared>) -> std::io::Result<()> {
    // Сервер однопоточный: одно зависшее соединение иначе съело бы веб-выход
    // до конца сессии. Для петли двух секунд хватает с запасом.
    let timeout = Some(std::time::Duration::from_secs(2));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);

    let path = read_request_path(&mut stream)?;

    // Галочка "Показывать HUD в OBS" (`web_enabled`) обязана реально
    // останавливать сервер, а не только прятать панель на уже работающей
    // странице - раньше отдавались и страница, и данные вне зависимости от
    // неё (претензия пользователя).
    let enabled = shared.lock().map(|s| s.config.web_enabled).unwrap_or(false);

    let (status, mime, body) = if !enabled {
        ("503 Service Unavailable", "text/plain; charset=utf-8", "web_enabled = false".to_string())
    } else {
        match path.as_str() {
            "/data.json" => {
                let body = match shared.lock() {
                    Ok(s) => to_json(&s.snapshot, &s.config, &s.twitch),
                    // Отравленный мьютекс значит, что render-поток паниковал; в
                    // релизе с `panic = "abort"` это недостижимо, но валиться тут
                    // из-за этого всё равно незачем.
                    Err(_) => "{}".to_string(),
                };
                ("200 OK", "application/json; charset=utf-8", body)
            }
            // Два маршрута на одну и ту же страницу: она сама смотрит на путь
            // и показывает нужную половину. Карточки покупок - ОТДЕЛЬНЫЙ
            // источник в OBS, который двигается независимо от статистики.
            "/hud" | "/toasts" => ("200 OK", "text/html; charset=utf-8", PAGE.to_string()),
            // Режима «всё вместе» больше нет (убран по запросу 2026-08-19): два
            // источника в OBS двигаются мышью, один - только настройками.
            // Корень отвечает не пустотой, а этими же двумя адресами: сюда
            // попадают, набрав адрес руками.
            "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", index_page()),
            _ => ("404 Not Found", "text/plain; charset=utf-8", "not found".to_string()),
        }
    };

    write!(
        stream,
        "HTTP/1.1 {status}\r\n\
         Content-Type: {mime}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body.as_bytes())?;
    stream.flush()?;
    // Закрываемся через FIN, а не через RST: Windows считает обрывом закрытие
    // сокета, у которого в приёмном буфере остался непрочитанный хвост
    // запроса, и стирает уже отправленный ответ - браузер не видит ничего.
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

/// Что показывать на корне: два адреса для OBS и ничего больше. Сюда попадают,
/// набрав адрес руками, и пустота читалась бы как «мод не работает».
fn index_page() -> String {
    let t = crate::i18n::t;
    format!(
        "<meta charset=\"utf-8\"><title>Game Information Counter</title>\
         <style>body{{background:#101014;color:#e8d7a8;font:16px/1.6 system-ui,sans-serif;padding:32px}}a{{color:#e8d7a8}}</style>\
         <h3>Game Information Counter</h3><p>{}</p>\
         <p><a href=\"/hud\">/hud</a> - {}<br><a href=\"/toasts\">/toasts</a> - {}</p>",
        t("Sources for an OBS Browser Source:"),
        t("stats"),
        t("viewer purchases"),
    )
}

/// Заголовки до пустой строки. Тела у GET нет, но дочитать их надо целиком:
/// недочитанный хвост ломает закрытие соединения (см. `serve`). Браузер и OBS
/// шлют заголовков заметно больше килобайта, так что одного `read` не хватает.
fn read_request_path(stream: &mut TcpStream) -> std::io::Result<String> {
    /// Предел на заголовки - чтобы соединение, которое льёт байты и не
    /// заканчивает, не съело память.
    const MAX_HEAD: usize = 8 * 1024;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < MAX_HEAD && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf);
    Ok(head.split_whitespace().nth(1).unwrap_or("/").to_string())
}

/// Покупки для страницы: `[{"viewer":"...","label":"...","cost":N,"age":1.2}]`.
///
/// Возраст в секундах, а не время события: часы браузера и игры не связаны, и
/// страница сама решает, когда карточке погаснуть.
///
/// Ник зрителя и название награды экранируются обязательно. Это не та же
/// осторожность, что с именем босса, а строже: там строку подменяет мод, здесь
/// её ПИШЕТ произвольный зритель.
fn twitch_json(t: &TwitchWeb, life_secs: f32) -> String {
    let items: Vec<String> = t
        .recent
        .iter()
        .filter_map(|p| {
            let age = p.at.elapsed().as_secs_f32();
            let life = p.life_secs(life_secs);
            (age < life).then(|| {
                // `wait` - сколько осталось до появления врага. Считается тут,
                // а не на странице: у неё нет часов игрового потока, а
                // отсчёт обязан идти в обоих выводах одинаково.
                let wait = p
                    .countdown_to
                    .map(|at| at.saturating_duration_since(std::time::Instant::now()).as_secs_f32())
                    .filter(|left| *left > 0.05)
                    .unwrap_or(0.0);
                format!(
                    "{{\"viewer\":\"{}\",\"label\":\"{}\",\"cost\":{},\"age\":{:.2},\"wait\":{:.1},\"life\":{:.1}}}",
                    json_escape(&p.viewer),
                    json_escape(&p.label),
                    p.cost,
                    age,
                    wait,
                    life
                )
            })
        })
        .collect();
    // `life` отдаём и общий, и у каждой карточки свой: страница обязана гасить
    // их по тому же времени, что и оверлей, иначе два вывода разъедутся - как
    // уже было со стилем.
    format!(
        "{{\"connected\":{},\"life\":{},\"events\":[{}]}}",
        t.connected,
        life_secs,
        items.join(",")
    )
}

/// Подписи метрик для виджета OBS: `(ключ на странице, полная, короткая)`.
///
/// Значения страница считает сама из сырых полей - это осознанно, - а слова
/// берёт отсюда. Свой словарь у неё был второй копией этих же строк и
/// разъезжался с панелью при каждой правке; вдобавок он знал ровно два языка,
/// а файлов локали может быть сколько угодно.
///
/// Строки те же, что в `overlay::content`, поэтому и переводятся одной
/// строкой файла на оба вывода.
pub const WEB_LABELS: &[(&str, &str, &str)] = &[
    ("deaths", "DEATHS", "deaths"),
    ("level", "LEVEL", "lvl"),
    ("runes", "RUNES", "runes"),
    ("runes_total", "RUNES TOTAL", "total"),
    ("playtime", "PLAYTIME", "time"),
    ("ng", "NG+", "NG+"),
    ("deathless", "DEATHLESS", "deathless"),
    ("boss_rate", "BOSSES / HOUR", "b/h"),
    ("death_rate", "DEATHS / HOUR", "d/h"),
    ("avg_attempts", "ATTEMPTS / BOSS", "att./boss"),
    ("deaths_on_boss", "DEATHS ON BOSSES", "boss deaths"),
    ("viewer_kills", "VIEWER KILLS", "by viewers"),
    ("map", "MAP", "map"),
    ("bosses", "BOSSES", "bosses"),
    ("attempt", "DEATHS##fight", "deaths##fight"),
    ("fight_time", "FIGHT TIME", "fight"),
    ("secs", "s", "s"),
];

/// `{"deaths":["DEATHS","deaths"], ...}` на языке, выбранном в настройках.
fn labels_json() -> String {
    let mut out = String::from("{");
    for (i, (key, full, short)) in WEB_LABELS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "\"{key}\":[\"{}\",\"{}\"]",
            json_escape(crate::i18n::t(full)),
            json_escape(crate::i18n::t(short))
        ));
    }
    out.push('}');
    out
}

fn to_json(s: &Snapshot, c: &Config, t: &TwitchWeb) -> String {
    let (killed, total) = match c.boss_count {
        BossCount::Named => s.bosses_named,
        BossCount::All => s.bosses_all,
    };
    let boss_name = s.boss_name.as_deref().unwrap_or("");
    format!(
        concat!(
            "{{\"valid\":{},\"hidden\":{},",
            "\"level\":{},\"deaths\":{},\"runes\":{},\"runes_total\":{},",
            "\"play_time_ms\":{},\"ng\":{},",
            "\"bosses_killed\":{},\"bosses_total\":{},",
            "\"boss_name\":\"{}\",\"boss_fight_active\":{},",
            "\"attempts\":{},\"attempt_secs\":{:.1},\"deaths_on_boss\":{},",
            "\"viewer_deaths\":{},",
            "\"near_boss\":\"{}\",\"near_dist\":{:.0},\"near_dy\":{:.0},",
            "\"total_attempts\":{},",
            "\"deathless_secs\":{:.0},",
            "\"map_visited\":{},\"map_total\":{},",
            "\"layout\":\"{}\",\"lang\":\"{}\",\"tr\":{},",
            "\"style\":{{\"label_size\":{},\"value_size\":{},\"counter_size\":{},",
            "\"boss_name_size\":{},\"attempt_size\":{},\"scale\":{},",
            "\"toast_label_size\":{},\"toast_value_size\":{},\"toast_width\":{},",
            "\"tracking\":{},\"line_gap\":{},",
            "\"accent\":\"#{}\",\"label_color\":\"#{}\",\"value_color\":\"#{}\",\"opacity\":{},",
            "\"border_opacity\":{},\"chassis\":\"{}\",",
            "\"rounding\":{},\"border_width\":{},",
            "\"toast_chassis\":\"{}\",\"toast_opacity\":{},\"toast_border_opacity\":{},",
            "\"toast_rounding\":{},\"toast_border_width\":{}}},",
            "\"show\":{{\"level\":{},\"deaths\":{},\"bosses\":{},\"boss_name\":{},",
            "\"all_boss_names\":{},",
            "\"attempts\":{},\"fight_timer\":{},\"runes\":{},",
            "\"runes_total\":{},\"playtime\":{},\"ng\":{},\"deathless\":{},",
            "\"deaths_on_boss\":{},\"boss_kill_rate\":{},\"death_rate\":{},\"avg_attempts\":{},",
            "\"viewer_kills\":{},\"nearest_boss\":{},",
            "\"boss_bar\":{},\"map_explored\":{}}},",
            "\"twitch\":{}}}"
        ),
        s.valid,
        // То же условие, что прячет оверлей в игре (`StreamHud::render`):
        // меню скрывает, если это включено в .ini (по умолчанию да), катсцена
        // - так же. Раньше меню было зашито без настройки, а виджет учитывал
        // только катсцену и оставался висеть поверх инвентаря, пока оверлей в
        // игре уже гас.
        (c.hide_in_menu && s.menu_open) || (c.hide_in_cutscene && s.in_cutscene),
        s.level,
        s.deaths,
        s.runes,
        s.runes_total,
        s.play_time_ms,
        s.ng_lvl,
        killed,
        total,
        json_escape(boss_name),
        s.boss_fight_active,
        s.attempts,
        s.attempt_secs,
        s.deaths_on_boss,
        s.viewer_deaths,
        // Имя приходит из игровых данных, которые может подменить любой мод -
        // экранируется обязательно, как и имя босса.
        json_escape(s.nearest_boss.as_ref().map_or("", |(n, _, _)| n.as_str())),
        s.nearest_boss.as_ref().map_or(0.0, |(_, d, _)| *d),
        s.nearest_boss.as_ref().map_or(0.0, |(_, _, dy)| *dy),
        s.total_attempts,
        s.deathless_secs,
        s.map_explored.0,
        s.map_explored.1,
        c.web_layout.as_key(),
        // Язык виджета тот же, что у окна настроек и панели: страница держит
        // свой словарь и выбирает по этому коду.
        crate::i18n::active(),
        labels_json(),
        // Кегли, масштаб и прозрачность у виджета свои: он висит поверх сцены
        // OBS, а не поверх игры, и общие с оверлеем числа туда почти никогда
        // не подходят.
        c.web_label_size,
        c.web_value_size,
        c.web_counter_size,
        c.web_boss_name_size,
        c.web_attempt_size,
        c.web_scale,
        // Карточка покупки в OBS - отдельный источник (`/toasts`), и размер у
        // неё свой: она стоит в углу сцены, а не рядом со статистикой.
        c.web_toast_label_size,
        c.web_toast_value_size,
        c.web_toast_width,
        // Разрядка и межстрочный - те же настройки, что у панели в игре,
        // просто своим числом: иначе два вывода снова разъедутся.
        c.web_tracking,
        c.web_line_gap,
        color_to_hex(c.accent_color),
        color_to_hex(c.label_color),
        color_to_hex(c.value_color),
        c.web_opacity,
        c.web_border_opacity,
        // Свой, не общий с игрой: поверх сцены OBS удачным оказывается
        // другое, чем поверх геймплея (прямой запрос 2026-08-20).
        c.web_panel_style.as_key(),
        // Форма корпуса - тоже настройка, а не константа страницы: иначе два
        // вывода разъедутся, как уже было со стилем.
        c.web_panel_rounding,
        c.web_border_width,
        // Корпус карточки покупки - свой и здесь: она отдельный источник и
        // висит поверх сцены, а не рядом со статистикой.
        c.web_toast_style.as_key(),
        c.web_toast_opacity,
        c.web_toast_border_opacity,
        c.web_toast_rounding,
        c.web_toast_border_width,
        c.show_level,
        c.show_deaths,
        c.show_bosses,
        c.show_boss_name,
        c.show_all_boss_names,
        c.show_attempts,
        c.show_fight_timer,
        c.show_runes,
        c.show_runes_total,
        c.show_playtime,
        c.show_ng,
        c.show_deathless,
        c.show_deaths_on_boss,
        c.show_boss_kill_rate,
        c.show_death_rate,
        c.show_avg_attempts,
        c.show_viewer_kills,
        c.show_nearest_boss,
        c.show_boss_bar,
        c.show_map_explored,
        twitch_json(t, c.twitch_notify_secs),
    )
}

/// Имя босса приходит из игровых данных, которые может подменить любой мод, -
/// это единственная строка в JSON, и экранировать её обязательно, иначе кавычка
/// в имени ломает разбор всей страницы.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Виджет в OBS обязан говорить на том же языке, что и панель в игре, а
    /// узнать его ему больше неоткуда. Значение не проверяем: язык глобальный,
    /// и параллельные тесты его двигают.
    #[test]
    fn json_carries_the_interface_language() {
        let json = to_json(&Snapshot::default(), &Config::default(), &TwitchWeb::default());
        assert!(json.contains("\"lang\":\""), "{json}");
    }

    #[test]
    fn escapes_what_would_break_the_page() {
        assert_eq!(json_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(json_escape("a\nb"), "a\\nb");
        assert_eq!(json_escape("\u{1}"), "\\u0001");
        // Юникод проходит как есть - страница отдаётся в UTF-8.
        assert_eq!(json_escape("Маргит"), "Маргит");
    }

    /// Заодно проверяет, что по пути из запроса ничего с диска не отдаётся:
    /// маршрутов ровно два, всё остальное - 404.
    #[test]
    fn serves_page_json_and_nothing_else() {
        // Виджет по умолчанию выключен, а тут проверяются маршруты - за
        // саму галочку отвечает `web_enabled_false_stops_serving`.
        let config = Config { web_enabled: true, ..Config::default() };
        let shared = Arc::new(Mutex::new(Shared {
            snapshot: Snapshot::default(),
            config,
            twitch: TwitchWeb::default(),
        }));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let served = Arc::clone(&shared);
        let server = std::thread::spawn(move || {
            for _ in 0..5 {
                let (stream, _) = listener.accept().unwrap();
                let _ = serve(stream, &served);
            }
        });

        let get = |path: &str| {
            let mut c = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            write!(c, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut out = String::new();
            c.read_to_string(&mut out).unwrap();
            out
        };

        // Корень - только адреса источников, самой панели там больше нет.
        let root = get("/");
        assert!(root.contains("/hud") && root.contains("/toasts"));
        assert!(!root.contains("data.json"), "режим «всё вместе» убран");
        assert!(get("/hud").contains("Game Information Counter"));
        assert!(get("/toasts").contains("Game Information Counter"));
        assert!(get("/data.json").contains("\"bosses_total\""));
        assert!(get("/../Cargo.toml").starts_with("HTTP/1.1 404"));
        server.join().unwrap();
    }

    /// Виджет обязан гаснуть при том же условии, что и оверлей в игре -
    /// иначе OBS показывает панель поверх инвентаря, пока в игре она уже
    /// спрятана.
    #[test]
    fn hides_in_menu_same_as_overlay() {
        let c = Config::default();
        let mut s = Snapshot { valid: true, ..Snapshot::default() };

        s.menu_open = true;
        assert!(to_json(&s, &c, &TwitchWeb::default()).contains("\"hidden\":true"));

        s.menu_open = false;
        assert!(to_json(&s, &c, &TwitchWeb::default()).contains("\"hidden\":false"));

        s.in_cutscene = true;
        assert!(to_json(&s, &c, &TwitchWeb::default()).contains("\"hidden\":true"));

        let mut c2 = c.clone();
        c2.hide_in_cutscene = false;
        assert!(to_json(&s, &c2, &TwitchWeb::default()).contains("\"hidden\":false"));
    }

    /// `hide_in_menu = false` держит виджет видимым в меню - раньше это было
    /// зашито без настройки.
    #[test]
    fn hide_in_menu_can_be_turned_off() {
        let mut c = Config::default();
        c.hide_in_menu = false;
        let s = Snapshot { valid: true, menu_open: true, ..Snapshot::default() };
        assert!(to_json(&s, &c, &TwitchWeb::default()).contains("\"hidden\":false"));
    }

    /// Галочка "Показывать HUD в OBS" обязана останавливать сервер целиком -
    /// раньше `web_enabled = false` ни на что не влияло: страница и данные
    /// отдавались как обычно.
    #[test]
    fn web_enabled_false_stops_serving() {
        let mut config = Config::default();
        config.web_enabled = false;
        let shared = Arc::new(Mutex::new(Shared {
            snapshot: Snapshot::default(),
            config,
            twitch: TwitchWeb::default(),
        }));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let served = Arc::clone(&shared);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = serve(stream, &served);
        });

        let mut c = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        write!(c, "GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 503"), "{out}");
        assert!(!out.contains("Game Information Counter"), "{out}");
        server.join().unwrap();
    }

    #[test]
    fn json_has_the_shape_the_page_expects() {
        let mut s = Snapshot::default();
        s.valid = true;
        s.level = 42;
        s.bosses_named = (3, 165);
        s.bosses_all = (9, 400);
        s.boss_name = Some("Margit, the \"Fell\" Omen".into());
        let c = Config::default();

        let json = to_json(&s, &c, &TwitchWeb::default());
        assert!(json.starts_with('{') && json.ends_with('}'));
        assert!(json.contains("\"level\":42"));
        // Стиль обязан ехать в страницу: иначе оверлей и OBS расходятся по
        // кеглям и цветам, как было живьём 2026-08-18.
        assert!(json.contains("\"style\""), "{json}");
        assert!(json.contains("\"layout\":\"d\""), "{json}");
        // Кегли виджета - свои поля: старое `title_size` их не покрывает.
        assert!(json.contains("\"counter_size\""), "{json}");
        assert!(json.contains("\"boss_name_size\""), "{json}");
        assert!(json.contains("\"attempt_size\""), "{json}");
        assert!(json.contains("\"accent\":\"#DEB870FF\""), "{json}");
        // boss_count = all по умолчанию.
        assert!(json.contains("\"bosses_killed\":9"), "{json}");
        assert!(json.contains("\"bosses_total\":400"), "{json}");
        assert!(json.contains(r#"the \"Fell\" Omen"#), "{json}");

        let mut named = c.clone();
        named.boss_count = BossCount::Named;
        let json = to_json(&s, &named, &TwitchWeb::default());
        assert!(json.contains("\"bosses_killed\":3"), "{json}");
        assert!(json.contains("\"bosses_total\":165"), "{json}");
    }

    /// Ник зрителя и название награды пишет посторонний человек - это самая
    /// недоверенная строка во всём выводе, строже даже имени босса.
    #[test]
    fn viewer_names_are_escaped() {
        let t = TwitchWeb {
            connected: true,
            recent: vec![crate::twitch::PurchaseEvent {
                viewer: r#"</script><img src=x onerror=alert(1)>"#.into(),
                label: "кавычка \" и слеш \\".into(),
                cost: 100,
                at: std::time::Instant::now(),
                countdown_to: None,
                life: None,
                redemption_id: String::new(),
            }],
        };
        let json = twitch_json(&t, 10.0);
        assert!(json.contains(r#""connected":true"#), "{json}");
        // Кавычка обязана уехать экранированной, иначе она рвёт весь JSON.
        assert!(json.contains(r#"кавычка \" и слеш \\"#), "{json}");
        assert!(json.contains("onerror"), "текст сохраняется, экранируется только разметка JSON");
    }

    /// Протухшие покупки не должны висеть на странице вечно.
    #[test]
    fn old_purchases_drop_out() {
        let old = std::time::Instant::now() - std::time::Duration::from_secs(30);
        let t = TwitchWeb {
            connected: false,
            recent: vec![crate::twitch::PurchaseEvent {
                viewer: "a".into(),
                label: "b".into(),
                cost: 1,
                at: old,
                countdown_to: None,
                life: None,
                redemption_id: String::new(),
            }],
        };
        assert!(twitch_json(&t, 6.0).contains(r#""events":[]"#));
    }

    /// Карта - сырые числа, процент считает сама страница, тем же приёмом,
    /// что и остальные комбинированные метрики.
    /// Корпус обязан доехать до страницы: иначе виджет в OBS останется с
    /// прежней рамкой, пока панель в игре уже переключилась - те самые
    /// разъехавшиеся выводы, от которых спасает общий `style`.
    #[test]
    fn card_and_spacing_reach_the_page() {
        // Правило то же, что у `style` вообще: настройка вида, не доехавшая до
        // страницы, - это два вывода, которые разъехались.
        let s = Snapshot { valid: true, ..Snapshot::default() };
        let mut c = Config::default();
        c.web_toast_label_size = 11.0;
        c.web_toast_value_size = 13.0;
        c.web_toast_width = 300.0;
        c.web_tracking = 2.5;
        c.web_line_gap = 9.0;
        let json = to_json(&s, &c, &TwitchWeb::default());
        for want in ["\"toast_label_size\":11", "\"toast_value_size\":13", "\"toast_width\":300", "\"tracking\":2.5", "\"line_gap\":9"] {
            assert!(json.contains(want), "{want} не доехало: {json}");
        }
    }

    #[test]
    fn chassis_reaches_the_page() {
        let s = Snapshot { valid: true, ..Snapshot::default() };
        for (style, key) in [
            (crate::config::PanelStyle::Frame, "frame"),
            (crate::config::PanelStyle::Bare, "bare"),
            (crate::config::PanelStyle::Bar, "bar"),
            (crate::config::PanelStyle::BarRight, "bar_right"),
        ] {
            let mut c = Config::default();
            c.web_panel_style = style;
            let json = to_json(&s, &c, &TwitchWeb::default());
            assert!(json.contains(&format!("\"chassis\":\"{key}\"")), "{key}: {json}");
        }
    }

    /// Форма корпуса - тоже настройка, и своя у каждого вывода: без неё
    /// виджет остался бы с прежними скруглением и толщиной, пока панель в
    /// игре уже поменялась (то же правило, что и для `chassis`).
    #[test]
    fn chassis_shape_reaches_the_page() {
        let s = Snapshot { valid: true, ..Snapshot::default() };
        let mut c = Config::default();
        c.web_panel_rounding = 3.5;
        c.web_border_width = 4.5;
        // Значения панели в игре другие - в JSON обязаны уехать именно веб-.
        c.panel_rounding = 20.0;
        c.border_width = 20.0;
        let json = to_json(&s, &c, &TwitchWeb::default());
        assert!(json.contains("\"rounding\":3.5"), "{json}");
        assert!(json.contains("\"border_width\":4.5"), "{json}");
    }

    #[test]
    fn map_progress_is_in_the_json() {
        let mut s = Snapshot { valid: true, ..Snapshot::default() };
        s.map_explored = (7, 60);
        let mut c = Config::default();
        c.show_map_explored = true;

        let json = to_json(&s, &c, &TwitchWeb::default());
        assert!(json.contains("\"map_visited\":7"), "{json}");
        assert!(json.contains("\"map_total\":60"), "{json}");
        assert!(json.contains("\"map_explored\":true"), "{json}");
    }
}

