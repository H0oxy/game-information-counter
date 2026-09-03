//! Диагностика по УЖЕ выданному токену: `cargo run --example twitch_probe`.
//!
//! Отличие от `twitch_check`: тот гоняет device flow с нуля и требует, чтобы
//! человек ввёл код на сайте. Этот берёт `game_information_counter.twitch` и отвечает на
//! вопрос «почему Twitch отказал», не трогая ничего: только GET-запросы,
//! никаких созданий и отмен.
//!
//! Путь к файлу - первым аргументом, иначе ищется рядом с exe примера.
//!
//! Токен НЕ печатается ни в каком виде: он равносилен паролю от канала.

use std::path::PathBuf;

use game_information_counter::twitch::{chat, eventsub, http, json, Event};

fn read_token(path: &PathBuf) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "access_token" {
                let v = value.trim().to_string();
                return (!v.is_empty()).then_some(v);
            }
        }
    }
    None
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("game_information_counter.twitch"));

    let Some(token) = read_token(&path) else {
        println!("нет access_token в {}", path.display());
        return;
    };
    println!("токен прочитан из {} ({} символов)", path.display(), token.len());

    // 1. validate - единственный запрос, которому не нужен client_id: он сам
    //    его и возвращает, вместе с реально выданными правами.
    println!("\n[1] /oauth2/validate");
    let auth = [("Authorization", format!("OAuth {token}"))];
    let Some(r) = http::request("id.twitch.tv", "GET", "/oauth2/validate", &auth, None).await else {
        println!("  нет связи с id.twitch.tv");
        return;
    };
    println!("  HTTP {}", r.status);
    if !r.ok() {
        println!("  тело: {}", r.body);
        return;
    }
    let client_id = json::str_field(&r.body, "client_id").unwrap_or_default();
    let login = json::str_field(&r.body, "login").unwrap_or_default();
    let user_id = json::str_field(&r.body, "user_id").unwrap_or_default();
    let expires_in = json::i64_field(&r.body, "expires_in").unwrap_or(0);
    println!("  канал:     {login} (id {user_id})");
    println!("  client_id: {client_id}");
    println!("  жить ещё:  {} мин", expires_in / 60);
    // Права - главное: 403 на наградах бывает и от их нехватки.
    let scopes = json::array_items(&r.body, "scopes");
    println!("  права:     {}", if scopes.is_empty() { "(пусто)".into() } else { scopes.join(" ") });

    let bearer = [
        ("Client-Id", client_id.clone()),
        ("Authorization", format!("Bearer {token}")),
    ];

    // 2. Профиль - тот же запрос, что мод делает при подключении.
    println!("\n[2] GET /helix/users");
    match http::request("api.twitch.tv", "GET", "/helix/users", &bearer, None).await {
        Some(r) => {
            println!("  HTTP {}", r.status);
            if let Some(item) = json::array_items(&r.body, "data").first() {
                println!(
                    "  тип канала: {:?}, партнёрство: {:?}",
                    json::str_field(item, "broadcaster_type").unwrap_or_default(),
                    json::str_field(item, "type").unwrap_or_default()
                );
            }
        }
        None => println!("  нет связи"),
    }

    // 3. Награды - ЧИТАЕМ, а не создаём. 403 здесь и 403 на создании имеют
    //    одну и ту же причину, но этот запрос ничего не меняет на канале.
    println!("\n[3] GET /helix/channel_points/custom_rewards");
    let path = format!("/helix/channel_points/custom_rewards?broadcaster_id={user_id}");
    match http::request("api.twitch.tv", "GET", &path, &bearer, None).await {
        Some(r) => {
            println!("  HTTP {}", r.status);
            let message = json::str_field(&r.body, "message").unwrap_or_default();
            if !message.is_empty() {
                println!("  Twitch: {message}");
            }
            if r.ok() {
                let items = json::array_items(&r.body, "data");
                println!("  наград на канале: {}", items.len());
                for item in items {
                    println!(
                        "    - {:?} за {} баллов",
                        json::str_field(item, "title").unwrap_or_default(),
                        json::i64_field(item, "cost").unwrap_or(0)
                    );
                }
            }
        }
        None => println!("  нет связи"),
    }

    // 4. Подписка на погашения - то же, что делает мод после welcome. Сессия
    //    закрывается сразу, подписка вместе с ней: она живёт ровно столько,
    //    сколько открыт сокет.
    println!("\n[4] EventSub: подписка на погашения наград");
    match eventsub::probe_subscription(&client_id, &token, &user_id).await {
        Ok(()) => println!("  подписка принята - погашения будут приходить"),
        Err(0) => println!("  не дошли до ответа (сеть или таймаут)"),
        Err(403) => println!("  HTTP 403 - у канала нет баллов канала (нужен Affiliate)"),
        Err(code) => println!("  HTTP {code}"),
    }

    // 5. Чат: источник никнеймов над врагами. Авторизации не требует вовсе,
    //    поэтому проверяется отдельно от всего выше.
    let channel = std::env::args().nth(2);
    if let Some(raw) = channel {
        println!("\n[5] чат канала {raw}");
        match chat::channel_name(&raw) {
            Some(name) => {
                let (tx, rx) = std::sync::mpsc::channel();
                chat::spawn(name.clone(), tx);
                // Третьим аргументом - сколько секунд слушать. Смысл именно в
                // кривой роста: список набирается из тех, кто ПИШЕТ, и по
                // скорости видно, упирается ли он в кап или в сам чат.
                let secs: u64 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(20);
                println!("  подключаемся к #{name}, слушаем {secs} с...");
                let started = std::time::Instant::now();
                let deadline = started + std::time::Duration::from_secs(secs);
                let mut best = 0usize;
                while std::time::Instant::now() < deadline {
                    if let Ok(Event::ChattersUpdated { list, .. }) = rx.recv_timeout(std::time::Duration::from_secs(2)) {
                        if list.len() > best {
                            best = list.len();
                            println!("  {:>4} с: {best} зрителей", started.elapsed().as_secs());
                        }
                    }
                }
                if best == 0 {
                    println!("  ни одного зрителя за {secs} с - канал офлайн или пуст");
                } else {
                    println!(
                        "  итого {best} за {secs} с (~{:.1}/мин); кап {}",
                        best as f64 * 60.0 / secs as f64,
                        game_information_counter::twitch::chat::MAX_TRACKED
                    );
                }
            }
            None => println!("  не похоже на имя канала"),
        }
    } else {
        println!("\n[5] чат пропущен - вторым аргументом можно передать канал");
    }
}
