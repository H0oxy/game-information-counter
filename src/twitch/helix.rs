//! Helix REST: свой user_id, подписка на события, создание награды и возврат
//! баллов.
//!
//! Список зрителей берётся отсюда, когда получается: `helix/chat/chatters`
//! отдаёт ВСЕХ, включая молчащих, а анонимный чат (`chat.rs`) - только тех,
//! кто пишет. Цена - право `moderator:read:chatters`, зашитое в уже выданный
//! токен: включить его постфактум нельзя без «Забыть авторизацию». Поэтому
//! чат остаётся запасным путём, а не заменяется.

use super::http;
use super::json;

const HOST: &str = "api.twitch.tv";

#[derive(PartialEq, Debug)]
pub enum HelixError {
    /// Сеть или неожиданный ответ - повторить позже.
    Network,
    /// 401: токен протух или отозван - обновить и повторить ОДИН раз.
    Unauthorized,
    /// HTTP-код и то, что Twitch написал в `message`.
    ///
    /// Текст нужен, потому что один код значит разное: 403 на баллах канала -
    /// это и «нет права `channel:manage:redemptions`», и «канал не Affiliate,
    /// баллов у него нет вовсе». Второе модом не чинится никак, и молчать об
    /// этом значит слать стримера переавторизовываться по кругу
    /// (жалоба 2026-08-19).
    Other(u16, String),
}

fn headers(client_id: &str, access_token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Client-Id", client_id.to_string()),
        ("Authorization", format!("Bearer {access_token}")),
        ("Content-Type", "application/json".to_string()),
    ]
}

fn classify(status: u16, body: &str) -> HelixError {
    match status {
        401 => HelixError::Unauthorized,
        s => HelixError::Other(s, json::str_field(body, "message").unwrap_or_default()),
    }
}

/// Id и логин владельца токена. Нужны как `broadcaster_user_id` для подписки и
/// как подпись «подключено как ...» в окне настроек.
pub async fn get_own_user(client_id: &str, access_token: &str) -> Result<(String, String), HelixError> {
    let r = http::request(HOST, "GET", "/helix/users", &headers(client_id, access_token), None)
        .await
        .ok_or(HelixError::Network)?;
    if !r.ok() {
        return Err(classify(r.status, &r.body));
    }
    let user = *json::array_items(&r.body, "data").first().ok_or(HelixError::Network)?;
    let id = json::str_field(user, "id").ok_or(HelixError::Network)?;
    let login = json::str_field(user, "login").unwrap_or_default();
    Ok((id, login))
}

/// Подписка на событие для уже открытой WebSocket-сессии.
///
/// `condition` - готовый JSON-объект вида `{"broadcaster_user_id":"123"}`:
/// у разных типов событий разные поля, и собирать их здесь в общем виде было
/// бы больше кода, чем передать строкой с места вызова.
pub async fn create_eventsub_subscription(
    client_id: &str,
    access_token: &str,
    sub_type: &str,
    version: &str,
    condition: &str,
    session_id: &str,
) -> Result<(), HelixError> {
    let body = format!(
        r#"{{"type":"{}","version":"{}","condition":{},"transport":{{"method":"websocket","session_id":"{}"}}}}"#,
        sub_type,
        version,
        condition,
        // Id сессии приходит от самого Twitch и всегда из букв, цифр и дефисов.
        // Фильтр здесь не «на всякий случай», а чтобы неожиданный ответ не мог
        // разломать JSON, который мы отправляем.
        sanitize_id(session_id),
    );
    let r = http::request(
        HOST,
        "POST",
        "/helix/eventsub/subscriptions",
        &headers(client_id, access_token),
        Some(&body),
    )
    .await
    .ok_or(HelixError::Network)?;
    if r.ok() {
        Ok(())
    } else {
        Err(classify(r.status, &r.body))
    }
}

/// Оставляет только то, из чего Twitch составляет идентификаторы. Кавычка,
/// скобка или обратный слеш здесь означали бы сломанный JSON запроса.
fn sanitize_id(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

/// Экранирование для JSON, который отправляем сами. Название награды пишет
/// стример, и кавычка в нём иначе разломала бы запрос.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
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

/// Создаёт награду за баллы канала на дашборде стримера.
///
/// Требует права `channel:manage:redemptions`. Возвращает id созданной награды -
/// его стоит запомнить, чтобы потом сопоставлять покупки точно, а не по имени.
///
/// 400 здесь чаще всего значит «награда с таким названием уже есть»: Twitch
/// требует уникальности, и это не ошибка мода.
pub async fn create_reward(
    client_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    title: &str,
    cost: u32,
    prompt: &str,
) -> Result<String, HelixError> {
    let body = format!(
        r#"{{"title":"{}","cost":{},"prompt":"{}","is_enabled":true,"should_redemptions_skip_request_queue":false}}"#,
        json_string(title),
        cost.max(1),
        json_string(prompt),
    );
    let path = format!("/helix/channel_points/custom_rewards?broadcaster_id={}", sanitize_id(broadcaster_id));
    let r = http::request(HOST, "POST", &path, &headers(client_id, access_token), Some(&body))
        .await
        .ok_or(HelixError::Network)?;
    if !r.ok() {
        return Err(classify(r.status, &r.body));
    }
    let item = *json::array_items(&r.body, "data").first().ok_or(HelixError::Network)?;
    json::str_field(item, "id").ok_or(HelixError::Network)
}

/// Приводит награду на Twitch в соответствие с тем, что настроено в моде.
///
/// Нужна, потому что цену и название правят в окне настроек, а на дашборде
/// они после этого остаются прежними: зритель платит старую цену за то, что
/// мод уже переименовал (жалоба 2026-08-23).
///
/// 404 здесь значит «награды больше нет» (стример удалил её руками) - это
/// стоит показать, а не молча проглотить.
///
/// **`is_enabled` НЕ отправляется, и это не забывчивость.** Галочка в моде
/// значит «мод исполняет эту награду», а не «награда доступна зрителям» -
/// доступностью распоряжается стример на дашборде. Пока мод слал туда свою
/// галочку, обновление цены гасило награду на Twitch (жалоба живьём
/// 2026-08-23). Одна настройка - один смысл.
pub async fn update_reward(
    client_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    reward_id: &str,
    title: &str,
    cost: u32,
) -> Result<(), HelixError> {
    // `should_redemptions_skip_request_queue` шлём ВСЕГДА и всегда `false` -
    // в отличие от `is_enabled`. Это не настройка стримера, а требование мода:
    // с пропуском очереди Twitch считает погашение выполненным сразу, и
    // вернуть за него баллы нельзя вообще ничем.
    let body = format!(
        r#"{{"title":"{}","cost":{},"should_redemptions_skip_request_queue":false}}"#,
        json_string(title),
        cost.max(1),
    );
    let path = format!(
        "/helix/channel_points/custom_rewards?broadcaster_id={}&id={}",
        sanitize_id(broadcaster_id),
        sanitize_id(reward_id),
    );
    let r = http::request(HOST, "PATCH", &path, &headers(client_id, access_token), Some(&body))
        .await
        .ok_or(HelixError::Network)?;
    if r.ok() {
        Ok(())
    } else {
        Err(classify(r.status, &r.body))
    }
}

/// Показывает награду зрителям или прячет её.
///
/// Единственное место, где мод шлёт `is_enabled` - и потому отдельная функция,
/// а не флаг у `update_reward`: там его нет намеренно (см. выше), иначе правка
/// цены гасила бы награду. Здесь доступность и есть предмет запроса, поэтому в
/// теле больше нет ничего - ни названия, ни цены.
pub async fn set_reward_enabled(
    client_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    reward_id: &str,
    on: bool,
) -> Result<(), HelixError> {
    let body = format!(r#"{{"is_enabled":{on}}}"#);
    let path = format!(
        "/helix/channel_points/custom_rewards?broadcaster_id={}&id={}",
        sanitize_id(broadcaster_id),
        sanitize_id(reward_id),
    );
    let r = http::request(HOST, "PATCH", &path, &headers(client_id, access_token), Some(&body))
        .await
        .ok_or(HelixError::Network)?;
    if r.ok() {
        Ok(())
    } else {
        Err(classify(r.status, &r.body))
    }
}

/// Убирает награду с дашборда.
///
/// Twitch разрешает удалять только награды, созданные ЭТИМ же приложением:
/// заведённую руками на дашборде мод удалить не может и получит 403.
/// 404 - её уже нет, и это не ошибка: результат ровно тот, которого хотели.
pub async fn delete_reward(
    client_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    reward_id: &str,
) -> Result<(), HelixError> {
    let path = format!(
        "/helix/channel_points/custom_rewards?broadcaster_id={}&id={}",
        sanitize_id(broadcaster_id),
        sanitize_id(reward_id),
    );
    let r = http::request(HOST, "DELETE", &path, &headers(client_id, access_token), None)
        .await
        .ok_or(HelixError::Network)?;
    if r.ok() || r.status == 404 {
        Ok(())
    } else {
        Err(classify(r.status, &r.body))
    }
}

/// Помечает погашение выполненным.
///
/// Без этого КАЖДАЯ успешно исполненная покупка остаётся у Twitch в статусе
/// «не выполнено» до конца времён, и отличить её от той, что мод проспал
/// (был выключен, отвалилась сеть), нельзя ничем. Именно на этом и держится
/// разбор хвоста в `unfulfilled`.
pub async fn fulfill(
    client_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    reward_id: &str,
    redemption_id: &str,
) -> Result<(), HelixError> {
    set_redemption_status(client_id, access_token, broadcaster_id, reward_id, redemption_id, "FULFILLED").await
}

async fn set_redemption_status(
    client_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    reward_id: &str,
    redemption_id: &str,
    status: &str,
) -> Result<(), HelixError> {
    let path = format!(
        "/helix/channel_points/custom_rewards/redemptions?id={}&broadcaster_id={}&reward_id={}",
        sanitize_id(redemption_id),
        sanitize_id(broadcaster_id),
        sanitize_id(reward_id),
    );
    let r = http::request(
        HOST,
        "PATCH",
        &path,
        &headers(client_id, access_token),
        Some(&format!(r#"{{"status":"{status}"}}"#)),
    )
    .await
    .ok_or(HelixError::Network)?;
    if r.ok() {
        Ok(())
    } else {
        Err(classify(r.status, &r.body))
    }
}

/// Покупки этой награды, которые никто не выполнил и не отменил.
///
/// Пока мод выключен или без связи, EventSub ему ничего не приносит и НЕ
/// переигрывает пропущенное потом. Такие покупки висят у Twitch навсегда:
/// баллы списаны, в игре не произошло ничего. Отсюда их и забираем при
/// подключении - чтобы вернуть.
///
/// ponytail: одна страница, до 50 штук за раз. Больше пропущенного за один
/// обрыв связи не набирается, а остаток подберётся при следующем подключении.
pub async fn unfulfilled(
    client_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    reward_id: &str,
) -> Result<Vec<String>, HelixError> {
    let path = format!(
        "/helix/channel_points/custom_rewards/redemptions?broadcaster_id={}&reward_id={}&status=UNFULFILLED&first=50",
        sanitize_id(broadcaster_id),
        sanitize_id(reward_id),
    );
    let r = http::request(HOST, "GET", &path, &headers(client_id, access_token), None)
        .await
        .ok_or(HelixError::Network)?;
    if !r.ok() {
        return Err(classify(r.status, &r.body));
    }
    Ok(redemption_ids(&r.body))
}

/// Id погашений из ответа. Строго верхнего уровня: `id` есть и у самого
/// погашения, и у награды внутри него, и перепутать их значит вернуть баллы
/// не за то (та же ловушка, что в `eventsub::parse_message`).
fn redemption_ids(body: &str) -> Vec<String> {
    json::array_items(body, "data").iter().filter_map(|item| json::str_field_top(item, "id")).collect()
}

/// Награды этого приложения, у которых включено «автоматически принимать»
/// (`should_redemptions_skip_request_queue`).
///
/// Стример может поставить эту галочку руками на дашборде, и тогда возврат
/// баллов у награды перестаёт работать молча: Twitch считает такое погашение
/// выполненным сразу, отменять нечего. Поэтому мод проверяет их при каждом
/// подключении и снимает галочку сам (запрос 2026-08-23).
///
/// `only_manageable_rewards=true` - только свои: чужие мы всё равно не
/// исправим, Twitch отдаёт правку лишь создавшему приложению.
pub async fn skipping_queue(
    client_id: &str,
    access_token: &str,
    broadcaster_id: &str,
) -> Result<Vec<String>, HelixError> {
    let path = format!(
        "/helix/channel_points/custom_rewards?broadcaster_id={}&only_manageable_rewards=true",
        sanitize_id(broadcaster_id),
    );
    let r = http::request(HOST, "GET", &path, &headers(client_id, access_token), None)
        .await
        .ok_or(HelixError::Network)?;
    if !r.ok() {
        return Err(classify(r.status, &r.body));
    }
    Ok(queue_skippers(&r.body))
}

/// Что у наград мода лежит на Twitch ПРЯМО СЕЙЧАС: id, название, цена.
///
/// Нужно ровно для метки синхронизации. Считать её по тому, что мы отправили,
/// нельзя: PATCH может не примениться (награду завели другим приложением,
/// её удалили с дашборда), и метка врала бы в обе стороны - «изменено» на
/// совпадающей и «на Twitch» на разъехавшейся (жалоба 2026-08-23).
pub async fn my_rewards(
    client_id: &str,
    access_token: &str,
    broadcaster_id: &str,
) -> Result<Vec<(String, String, u32)>, HelixError> {
    let path = format!(
        "/helix/channel_points/custom_rewards?broadcaster_id={}&only_manageable_rewards=true",
        sanitize_id(broadcaster_id),
    );
    let r = http::request(HOST, "GET", &path, &headers(client_id, access_token), None)
        .await
        .ok_or(HelixError::Network)?;
    if !r.ok() {
        return Err(classify(r.status, &r.body));
    }
    Ok(reward_states(&r.body))
}

/// Разбор ответа - отдельно от сети, на этом и держится тест.
fn reward_states(body: &str) -> Vec<(String, String, u32)> {
    json::array_items(body, "data")
        .iter()
        .filter_map(|item| {
            let id = json::str_field_top(item, "id")?;
            let title = json::str_field(item, "title")?;
            let cost = json::u32_field(item, "cost")?;
            Some((id, title, cost))
        })
        .collect()
}

/// Id наград с включённым «автоматически принимать». Отдельно от сети - на
/// этом и держится тест.
fn queue_skippers(body: &str) -> Vec<String> {
    json::array_items(body, "data")
        .iter()
        .filter(|item| json::bool_field(item, "should_redemptions_skip_request_queue") == Some(true))
        .filter_map(|item| json::str_field_top(item, "id"))
        .collect()
}

/// Снимает у награды «автоматически принимать» - иначе возврат баллов за неё
/// невозможен. Ничего другого не трогает.
pub async fn require_queue(
    client_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    reward_id: &str,
) -> Result<(), HelixError> {
    let path = format!(
        "/helix/channel_points/custom_rewards?broadcaster_id={}&id={}",
        sanitize_id(broadcaster_id),
        sanitize_id(reward_id),
    );
    let body = r#"{"should_redemptions_skip_request_queue":false}"#;
    let r = http::request(HOST, "PATCH", &path, &headers(client_id, access_token), Some(body))
        .await
        .ok_or(HelixError::Network)?;
    if r.ok() {
        Ok(())
    } else {
        Err(classify(r.status, &r.body))
    }
}

/// Отменяет погашение и возвращает зрителю баллы.
///
/// Единственный способ не обмануть зрителя, когда действие не сработало:
/// списывает баллы Twitch, и вернуть их может только он же.
///
/// Работает лишь для награды, у которой включена очередь подтверждения
/// (`should_redemptions_skip_request_queue = false`) - у пропускающих очередь
/// погашение сразу считается выполненным, и отменять нечего.
pub async fn refund(
    client_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    reward_id: &str,
    redemption_id: &str,
) -> Result<(), HelixError> {
    let path = format!(
        "/helix/channel_points/custom_rewards/redemptions?id={}&broadcaster_id={}&reward_id={}",
        sanitize_id(redemption_id),
        sanitize_id(broadcaster_id),
        sanitize_id(reward_id),
    );
    let r = http::request(
        HOST,
        "PATCH",
        &path,
        &headers(client_id, access_token),
        Some(r#"{"status":"CANCELED"}"#),
    )
    .await
    .ok_or(HelixError::Network)?;
    if r.ok() {
        Ok(())
    } else {
        Err(classify(r.status, &r.body))
    }
}

/// Все, кто сейчас в чате, - включая молчащих.
///
/// Требует права `moderator:read:chatters` и того, чтобы владелец токена был
/// модератором канала. Свой канал этому удовлетворяет всегда (стример -
/// модератор у себя), чужой - почти никогда, и тогда остаётся анонимный чат.
///
/// ponytail: одна страница, до 1000 имён. Для подписей над врагами нужны
/// единицы одновременно (слотов тегов у игры восемь), а пагинация ради
/// многотысячных каналов - это код, который никто не проверит.
pub async fn get_chatters(
    client_id: &str,
    access_token: &str,
    broadcaster_id: &str,
    moderator_id: &str,
) -> Result<Vec<String>, HelixError> {
    let path = format!(
        "/helix/chat/chatters?broadcaster_id={}&moderator_id={}&first=1000",
        sanitize_id(broadcaster_id),
        sanitize_id(moderator_id),
    );
    let r = http::request(HOST, "GET", &path, &headers(client_id, access_token), None)
        .await
        .ok_or(HelixError::Network)?;
    if !r.ok() {
        return Err(classify(r.status, &r.body));
    }
    Ok(chatters_from_json(&r.body))
}

/// Имена из ответа `chatters`. Отдельно от сети - на этом и держится тест.
fn chatters_from_json(body: &str) -> Vec<String> {
    json::array_items(body, "data")
        .iter()
        // `user_name` - отображаемое имя (с заглавными и не-латиницей), его и
        // показываем; на каналах с не-латинскими никами оно единственное,
        // которое зритель узнает.
        .filter_map(|item| {
            // Пустое отображаемое имя - не имя: берём логин, он есть всегда.
            json::str_field(item, "user_name")
                .filter(|n| !n.is_empty())
                .or_else(|| json::str_field(item, "user_login"))
                .filter(|n| !n.is_empty())
        })
        .collect()
}

/// Условие подписки на награды: события только нашего канала.
pub fn broadcaster_condition(user_id: &str) -> String {
    format!(r#"{{"broadcaster_user_id":"{}"}}"#, sanitize_id(user_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Награда с включённым «автоматически принимать» обязана находиться по
    /// ответу Twitch: с этой галочкой возврат баллов не работает, и мод
    /// снимает её сам.
    #[test]
    fn auto_accept_rewards_are_spotted() {
        let body = r#"{"data":[
            {"id":"a1","title":"обычная","should_redemptions_skip_request_queue":false},
            {"id":"b2","title":"авто","should_redemptions_skip_request_queue":true},
            {"id":"c3","title":"без поля"}
        ]}"#;
        assert_eq!(queue_skippers(body), vec!["b2".to_string()]);
        assert!(queue_skippers("{}").is_empty());
        // Название пишет стример, и «...":true» внутри него не должно
        // притворяться полем: строковые литералы экстрактор пропускает целиком.
        let forged = r#"{"data":[{"title":"\"should_redemptions_skip_request_queue\":true","id":"x","should_redemptions_skip_request_queue":false}]}"#;
        assert!(queue_skippers(forged).is_empty());
    }

    /// Метка синхронизации считается по тому, что РЕАЛЬНО лежит на Twitch,
    /// поэтому разбор ответа обязан давать и название, и цену. Без цены метка
    /// «на Twitch» стояла бы на награде, которой там уже поправили стоимость.
    #[test]
    fn reward_state_carries_title_and_cost() {
        let body = r#"{"data":[
            {"id":"a1","title":"Прыжок","cost":100},
            {"id":"b2","title":"Спавн","cost":5000}
        ]}"#;
        assert_eq!(
            reward_states(body),
            vec![
                ("a1".to_string(), "Прыжок".to_string(), 100),
                ("b2".to_string(), "Спавн".to_string(), 5000),
            ]
        );
        // Строка без цены - не строка: молча пропускаем, а не считаем ноль.
        assert!(reward_states(r#"{"data":[{"id":"a1","title":"Прыжок"}]}"#).is_empty());
        assert!(reward_states("{}").is_empty());
    }

    #[test]
    fn maps_status_to_error() {
        assert_eq!(classify(401, ""), HelixError::Unauthorized);
        assert_eq!(classify(500, ""), HelixError::Other(500, String::new()));
        // Текст Twitch доезжает до вызывающего: без него 403 на баллах канала
        // не отличить от 403 на правах.
        assert_eq!(
            classify(403, r#"{"error":"Forbidden","status":403,"message":"The broadcaster must have partner or affiliate status."}"#),
            HelixError::Other(403, "The broadcaster must have partner or affiliate status.".to_string())
        );
    }

    /// Название награды пишет человек - кавычка в нём не должна ломать запрос.
    #[test]
    fn titles_are_escaped_for_json() {
        assert_eq!(json_string(r#"Say "hi""#), r#"Say \"hi\""#);
        assert_eq!(json_string("a\\b"), r#"a\\b"#);
        assert_eq!(json_string("\u{1}"), r#"\u0001"#);
        assert_eq!(json_string("Прыжок"), "Прыжок");
    }

    /// Список зрителей: берём отображаемое имя, а без него - логин.
    #[test]
    fn chatters_are_read_from_the_answer() {
        let body = r#"{"data":[{"user_id":"1","user_login":"alpha","user_name":"Alpha"},
                                {"user_id":"2","user_login":"beta","user_name":""}],
                       "pagination":{},"total":2}"#;
        assert_eq!(chatters_from_json(body), vec!["Alpha".to_string(), "beta".to_string()]);
        // Пустой и мусорный ответ - пустой список, а не паника.
        assert!(chatters_from_json("{}").is_empty());
        assert!(chatters_from_json("").is_empty());
    }

    /// Хвост непогашенных: берём id самого погашения, а не награды внутри.
    #[test]
    fn unfulfilled_reads_the_redemption_id_not_the_reward_id() {
        let body = r#"{"data":[{"id":"red-1","user_name":"a","reward":{"id":"rew-9","title":"x"}},
                                {"id":"red-2","user_name":"b","reward":{"id":"rew-9","title":"x"}}]}"#;
        assert_eq!(redemption_ids(body), vec!["red-1".to_string(), "red-2".to_string()]);
        assert!(redemption_ids("{}").is_empty());
        assert!(redemption_ids("").is_empty());
    }

    /// Подделать структуру отправляемого JSON через id нельзя.
    #[test]
    fn ids_are_sanitized() {
        assert_eq!(sanitize_id("abc-123_XYZ"), "abc-123_XYZ");
        assert_eq!(sanitize_id(r#"a","transport":{"method":"webhook"#), "atransportmethodwebhook");
        assert_eq!(broadcaster_condition("42"), r#"{"broadcaster_user_id":"42"}"#);
    }
}
