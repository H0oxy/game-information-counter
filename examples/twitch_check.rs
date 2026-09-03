//! Проверка подключения к Twitch без игры.
//!
//! Внутри игры диагностировать нечем: консоли нет, лога нет, а «подключаемся...»
//! по кругу одинаково выглядит и при опечатке в Client ID, и при неверном
//! Client Type, и при отсутствии Affiliate. Здесь те же самые функции, что
//! работают в моде, но с выводом на каждом шаге.
//!
//! ```
//! cargo run --example twitch_check -- <client_id>
//! ```
//!
//! Права запрашиваются те же, что у мода. Токен НЕ сохраняется: это разовая
//! проверка, а не вторая точка входа.

use std::time::{Duration, Instant};

use game_information_counter::twitch::{auth, eventsub, helix, http};

fn main() {
    let Some(client_id) = std::env::args().nth(1) else {
        eprintln!("нужен Client ID: cargo run --example twitch_check -- <client_id>");
        std::process::exit(2);
    };
    let client_id = client_id.trim().to_string();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("не удалось поднять tokio");
    let code = rt.block_on(check(&client_id));
    std::process::exit(code);
}

async fn check(client_id: &str) -> i32 {
    println!("Client ID: {client_id} ({} символов)", client_id.chars().count());
    if client_id.chars().any(|c| !c.is_ascii_alphanumeric()) {
        println!("  ВНИМАНИЕ: обычно Client ID - это 30 символов из букв и цифр.");
        println!("  Лишние пробелы или кавычки Twitch не примет.");
    }

    // Шаг 0 отделяет «мод не умеет в сеть» от «Twitch не принял данные»:
    // дальше все шаги идут по тому же HTTPS-клиенту.
    println!("\n[1/4] связь с id.twitch.tv");
    match http::request("id.twitch.tv", "GET", "/oauth2/validate", &[], None).await {
        Some(r) => println!("  ok, HTTP {}", r.status),
        None => {
            println!("  НЕТ СВЯЗИ. Сеть, файрвол, VPN или античит режут исходящий HTTPS.");
            return 1;
        }
    }

    println!("\n[2/4] запрос кода устройства");
    let device = match auth::request_device_code(client_id, game_information_counter::twitch::SCOPE_REDEMPTIONS).await {
        Ok(d) => {
            println!("  ok");
            println!("  ОТКРОЙ: {}", d.verification_uri);
            println!("  КОД:    {}", d.user_code);
            d
        }
        Err(why) => {
            println!("  ОТКАЗ: {why}");
            println!("\n  Что проверить:");
            println!("   - Client Type приложения обязан быть Public (не Confidential);");
            println!("   - Client ID скопирован целиком, без пробелов;");
            println!("   - приложение не удалено на dev.twitch.tv/console/apps.");
            return 1;
        }
    };

    println!("\n[3/4] ждём подтверждения (до 5 минут, Ctrl+C чтобы прервать)");
    let deadline = Instant::now() + Duration::from_secs(300);
    let token = loop {
        if Instant::now() >= deadline {
            println!("  не дождались подтверждения");
            return 1;
        }
        tokio::time::sleep(device.interval).await;
        match auth::poll_token(client_id, &device.device_code).await {
            auth::PollOutcome::Token(t) => break t,
            auth::PollOutcome::Pending => print!("."),
            auth::PollOutcome::SlowDown => println!("  Twitch просит опрашивать реже"),
            auth::PollOutcome::Denied => {
                println!("  доступ отклонён на сайте");
                return 1;
            }
            auth::PollOutcome::Expired => {
                println!("  код истёк, запусти проверку заново");
                return 1;
            }
            auth::PollOutcome::Failed => println!("  неожиданный ответ, повторяем"),
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    };
    println!("\n  ok, токен получен");

    println!("\n[4/4] профиль и подписка на баллы канала");
    let (user_id, login) = match helix::get_own_user(client_id, &token.access_token).await {
        Ok(v) => {
            println!("  профиль: {} (id {})", v.1, v.0);
            v
        }
        Err(e) => {
            println!("  не удалось прочитать профиль: {e:?}");
            return 1;
        }
    };

    // Подписка требует открытой WS-сессии, поэтому проверяем целиком: только
    // так видно 403 у не-Affiliate, а это самая частая причина «всё настроил,
    // а событий нет».
    match eventsub::probe_subscription(client_id, &token.access_token, &user_id).await {
        Ok(()) => {
            println!("  ok: подписка на баллы канала создана");
            println!("\nВСЁ РАБОТАЕТ. В игре: тот же Client ID, twitch_enabled = true.");
            println!("Награду заводи на дашборде Twitch, гасить её можешь сам - зрители не нужны.");
            0
        }
        Err(code) if code == 403 => {
            println!("  ОТКАЗ 403: Twitch не даёт подписаться на баллы канала.");
            println!("\n  Почти наверняка канал {login} не Affiliate и не Partner.");
            println!("  Баллов канала у такого канала не существует - это ограничение Twitch,");
            println!("  а не мода. Показ карточек проверяется кнопкой «Тестовая покупка» в F7.");
            1
        }
        Err(code) => {
            println!("  ОТКАЗ: HTTP {code}");
            1
        }
    }
}
