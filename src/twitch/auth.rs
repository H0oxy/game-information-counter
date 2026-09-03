//! Авторизация: Device Code Flow.
//!
//! Выбран потому, что мод - публичный клиент: DLL лежит у пользователя на
//! диске, и Client Secret хранить в ней негде. Device flow secret'а и не
//! требует - стример открывает `twitch.tv/activate`, вводит код, мод получает
//! токен. Client ID при этом не секрет и спокойно живёт в `.ini`.
//!
//! Файл `game_information_counter.twitch` - единственный в моде, чья утечка чего-то
//! стоит: это доступ к каналу в рамках выданных прав. Отсюда предупреждение
//! прямо в шапке файла и минимальный набор прав (см. `mod.rs`).

use std::time::Duration;

use crate::config::dll_sibling;
use crate::stats::unix_now;

use super::http;
use super::json;

const HOST: &str = "id.twitch.tv";
const TOKEN_FILE: &str = "game_information_counter.twitch";

/// Обновляем токен заранее: если ждать самого истечения, первый же запрос
/// после него пойдёт с мёртвым токеном и потратит попытку впустую.
const RENEW_MARGIN_SECS: u64 = 300;

#[derive(Clone, PartialEq, Debug)]
pub struct TwitchToken {
    pub access_token: String,
    pub refresh_token: String,
    /// Момент истечения в unix-времени. `Instant` между запусками бессмыслен -
    /// тот же довод, что у `last_death` в stats.rs.
    pub expires_at: u64,
}

impl TwitchToken {
    pub fn needs_renewal(&self, now: u64) -> bool {
        self.expires_at <= now.saturating_add(RENEW_MARGIN_SECS)
    }
}

pub struct DeviceCode {
    pub device_code: String,
    /// Код, который стример вводит на сайте.
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: Duration,
    /// Как часто Twitch разрешает спрашивать «уже подтвердили?».
    pub interval: Duration,
}

/// Чем закончился один опрос токена.
#[derive(PartialEq, Debug)]
pub enum PollOutcome {
    /// Стример ещё не ввёл код.
    Pending,
    /// Спрашиваем слишком часто - Twitch просит притормозить.
    SlowDown,
    /// Отказано в доступе.
    Denied,
    /// Код протух, нужен новый.
    Expired,
    Token(TwitchToken),
    /// Сеть или неожиданный ответ - повторить позже.
    Failed,
}

// ---------------------------------------------------------------------------
// Разбор ответов - без сети, поэтому тестируется целиком
// ---------------------------------------------------------------------------

fn token_from_json(body: &str, now: u64) -> Option<TwitchToken> {
    let expires_in = json::i64_field(body, "expires_in").unwrap_or(0).max(0) as u64;
    Some(TwitchToken {
        access_token: json::str_field(body, "access_token")?,
        // При refresh Twitch присылает новый refresh_token - старый после
        // этого недействителен, поэтому берём именно из ответа.
        refresh_token: json::str_field(body, "refresh_token")?,
        expires_at: now.saturating_add(expires_in),
    })
}

fn device_from_json(body: &str) -> Option<DeviceCode> {
    Some(DeviceCode {
        device_code: json::str_field(body, "device_code")?,
        user_code: json::str_field(body, "user_code")?,
        verification_uri: json::str_field(body, "verification_uri")?,
        expires_in: Duration::from_secs(json::i64_field(body, "expires_in").unwrap_or(1800).max(0) as u64),
        // Меньше секунды Twitch не разрешает при всём желании.
        interval: Duration::from_secs(json::i64_field(body, "interval").unwrap_or(5).clamp(1, 60) as u64),
    })
}

/// Ответ на опрос токена. Пока пользователь не подтвердил, Twitch отвечает
/// 400 с текстом в `message` - различаем именно по нему, а не по коду.
fn poll_outcome(status: u16, body: &str, now: u64) -> PollOutcome {
    if (200..300).contains(&status) {
        return match token_from_json(body, now) {
            Some(t) => PollOutcome::Token(t),
            None => PollOutcome::Failed,
        };
    }
    let message = json::str_field(body, "message").unwrap_or_default().to_ascii_lowercase();
    if message.contains("authorization_pending") || message.contains("pending") {
        PollOutcome::Pending
    } else if message.contains("slow") {
        PollOutcome::SlowDown
    } else if message.contains("expired") {
        PollOutcome::Expired
    } else if message.contains("denied") || message.contains("declin") {
        PollOutcome::Denied
    } else {
        PollOutcome::Failed
    }
}

/// Percent-encoding для тела формы. Своё, потому что кодировать надо ровно два
/// значения (список прав и refresh-токен), а крейт ради этого - перебор.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Файл токена
// ---------------------------------------------------------------------------

/// Токены уезжают в файл зашифрованными (DPAPI, см. `crate::secret`): это
/// доступ к каналу, и лежать открытым рядом с DLL ему незачем. Файл с чужой
/// машины не расшифруется - мод просто попросит авторизоваться заново.
/// Токены уезжают в файл зашифрованными (DPAPI, см. `crate::secret`): это
/// доступ к каналу, и лежать открытым рядом с DLL ему незачем. Файл с чужой
/// машины не расшифруется - мод просто попросит авторизоваться заново.
fn encode_token(t: &TwitchToken) -> String {
    format!(
        "; Do not share this file and do not attach it to bug reports:\n; it is access to your Twitch channel, a password in effect.\n; The tokens are encrypted against your Windows account, so the\n; file is useless on any other machine.\n; Deleting it unlinks the mod - it will ask you to log in again.\naccess_token = {}\nrefresh_token = {}\nexpires_at = {}\n",
        crate::secret::protect(&t.access_token),
        crate::secret::protect(&t.refresh_token),
        t.expires_at
    )
}

fn decode_token(text: &str) -> Option<TwitchToken> {
    let mut access = String::new();
    let mut refresh = String::new();
    let mut expires_at = 0u64;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim().to_string();
        match key.trim() {
            // Старые файлы лежат открытым текстом - `reveal` отдаёт их как
            // есть, и следующее сохранение перепишет уже зашифрованным.
            "access_token" => access = crate::secret::reveal(&value),
            "refresh_token" => refresh = crate::secret::reveal(&value),
            "expires_at" => expires_at = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    // Без refresh_token токен одноразовый: как истечёт, восстановить нечем -
    // такой файл честнее считать отсутствующим.
    (!access.is_empty() && !refresh.is_empty()).then_some(TwitchToken {
        access_token: access,
        refresh_token: refresh,
        expires_at,
    })
}

pub fn load_token(hmodule: usize) -> Option<TwitchToken> {
    decode_token(&std::fs::read_to_string(dll_sibling(hmodule, TOKEN_FILE)?).ok()?)
}

pub fn save_token(hmodule: usize, t: &TwitchToken) {
    let Some(path) = dll_sibling(hmodule, TOKEN_FILE) else { return };
    crate::config::write_atomic(&path, &encode_token(t));
}

/// Токен отозван или протух насовсем - забываем, чтобы мод предложил
/// авторизоваться заново, а не бился в стену с мёртвым refresh.
pub fn forget_token(hmodule: usize) {
    if let Some(path) = dll_sibling(hmodule, TOKEN_FILE) {
        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// Сеть
// ---------------------------------------------------------------------------

fn form() -> [(&'static str, String); 1] {
    [("Content-Type", "application/x-www-form-urlencoded".to_string())]
}

/// Читаемая причина отказа. Диагностировать вслепую нечем: снаружи всё
/// выглядит как «подключаемся...» по кругу, а причин ровно три - не тот
/// Client ID, не тот Client Type и нет сети.
pub fn explain(status: u16, body: &str) -> String {
    let message = json::str_field(body, "message").unwrap_or_default();
    let low = message.to_ascii_lowercase();
    // Самая частая причина: приложение зарегистрировано как Confidential.
    // Device flow с ним не работает вовсе, а текст Twitch об этом молчит.
    if low.contains("client") && (low.contains("invalid") || low.contains("not found")) {
        return format!(
            "{} ({status}) - {}",
            crate::i18n::t("Twitch did not accept the Client ID"),
            crate::i18n::t("check it is copied in full and the Client Type is Public")
        );
    }
    if message.is_empty() {
        format!("{} {status}", crate::i18n::t("Twitch answered"))
    } else {
        format!("{} {status}: {message}", crate::i18n::t("Twitch answered"))
    }
}

pub async fn request_device_code(client_id: &str, scopes: &str) -> Result<DeviceCode, String> {
    let body = format!("client_id={}&scopes={}", urlencode(client_id), urlencode(scopes));
    let Some(r) = http::request(HOST, "POST", "/oauth2/device", &form(), Some(&body)).await else {
        return Err(crate::i18n::t("cannot reach id.twitch.tv (network, firewall or anti-cheat)").to_string());
    };
    if !r.ok() {
        return Err(explain(r.status, &r.body));
    }
    device_from_json(&r.body).ok_or_else(|| crate::i18n::t("Twitch answered without a device code").to_string())
}

pub async fn poll_token(client_id: &str, device_code: &str) -> PollOutcome {
    let body = format!(
        "client_id={}&device_code={}&grant_type=urn:ietf:params:oauth:grant-type:device_code",
        urlencode(client_id),
        urlencode(device_code)
    );
    match http::request(HOST, "POST", "/oauth2/token", &form(), Some(&body)).await {
        Some(r) => poll_outcome(r.status, &r.body, unix_now()),
        None => PollOutcome::Failed,
    }
}

/// `None` значит «обновить не вышло». Отличить «сеть отвалилась» от «токен
/// отозван» можно по флагу: при отзыве Twitch отвечает 4xx, и повторять
/// бессмысленно - нужен новый device code.
pub async fn refresh(client_id: &str, refresh_token: &str) -> Result<TwitchToken, RefreshError> {
    let body = format!(
        "client_id={}&refresh_token={}&grant_type=refresh_token",
        urlencode(client_id),
        urlencode(refresh_token)
    );
    let Some(r) = http::request(HOST, "POST", "/oauth2/token", &form(), Some(&body)).await else {
        return Err(RefreshError::Network);
    };
    if !r.ok() {
        // 4xx - refresh-токен больше не действителен (отозван, протух,
        // сменился пароль). 5xx - беда на той стороне, имеет смысл повторить.
        return Err(if (400..500).contains(&r.status) { RefreshError::Rejected } else { RefreshError::Network });
    }
    token_from_json(&r.body, unix_now()).ok_or(RefreshError::Rejected)
}

#[derive(PartialEq, Debug)]
pub enum RefreshError {
    /// Повторить позже.
    Network,
    /// Авторизоваться заново.
    Rejected,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_code_response() {
        let body = r#"{"device_code":"dc","expires_in":1800,"interval":5,
                       "user_code":"ABCD1234","verification_uri":"https://www.twitch.tv/activate"}"#;
        let d = device_from_json(body).unwrap();
        assert_eq!(d.device_code, "dc");
        assert_eq!(d.user_code, "ABCD1234");
        assert_eq!(d.interval, Duration::from_secs(5));
        assert_eq!(d.expires_in, Duration::from_secs(1800));
    }

    /// Интервал из ответа не должен превращаться в busy-loop, даже если
    /// придёт ноль или мусор.
    #[test]
    fn poll_interval_is_clamped() {
        let d = device_from_json(r#"{"device_code":"d","user_code":"u","verification_uri":"v","interval":0}"#).unwrap();
        assert_eq!(d.interval, Duration::from_secs(1));
        let d = device_from_json(r#"{"device_code":"d","user_code":"u","verification_uri":"v"}"#).unwrap();
        assert_eq!(d.interval, Duration::from_secs(5), "без поля - разумный дефолт");
    }

    #[test]
    fn successful_poll_yields_a_token() {
        let body = r#"{"access_token":"at","refresh_token":"rt","expires_in":100,"token_type":"bearer"}"#;
        let outcome = poll_outcome(200, body, 1_000);
        assert_eq!(
            outcome,
            PollOutcome::Token(TwitchToken {
                access_token: "at".into(),
                refresh_token: "rt".into(),
                expires_at: 1_100,
            })
        );
    }

    /// Пока стример не ввёл код, Twitch отвечает 400 - это не ошибка, а
    /// нормальный ход событий, и различается он по тексту, а не по коду.
    #[test]
    fn pending_is_not_an_error() {
        assert_eq!(
            poll_outcome(400, r#"{"status":400,"message":"authorization_pending"}"#, 0),
            PollOutcome::Pending
        );
        assert_eq!(poll_outcome(400, r#"{"message":"slow down"}"#, 0), PollOutcome::SlowDown);
        assert_eq!(poll_outcome(400, r#"{"message":"expired token"}"#, 0), PollOutcome::Expired);
        assert_eq!(poll_outcome(400, r#"{"message":"access denied"}"#, 0), PollOutcome::Denied);
        assert_eq!(poll_outcome(500, "", 0), PollOutcome::Failed);
        // 200 без токена - тоже неудача, а не паника.
        assert_eq!(poll_outcome(200, "{}", 0), PollOutcome::Failed);
    }

    #[test]
    fn token_file_round_trips() {
        let t = TwitchToken {
            access_token: "aaa".into(),
            refresh_token: "bbb".into(),
            expires_at: 12345,
        };
        assert_eq!(decode_token(&encode_token(&t)), Some(t));
    }

    /// Файл без refresh-токена восстановить нечем - считаем его отсутствующим,
    /// чтобы мод сразу предложил авторизацию, а не ждал истечения.
    #[test]
    fn token_without_refresh_is_rejected() {
        assert_eq!(decode_token("access_token = a\nexpires_at = 5\n"), None);
        assert_eq!(decode_token(""), None);
        assert_eq!(decode_token("мусор\nбез знака равно"), None);
    }

    #[test]
    fn renewal_happens_before_expiry() {
        let t = TwitchToken {
            access_token: "a".into(),
            refresh_token: "b".into(),
            expires_at: 1_000,
        };
        assert!(!t.needs_renewal(1_000 - RENEW_MARGIN_SECS - 1));
        assert!(t.needs_renewal(1_000 - RENEW_MARGIN_SECS));
        assert!(t.needs_renewal(2_000), "просроченный тоже требует обновления");
    }

    #[test]
    fn urlencodes_what_needs_it() {
        assert_eq!(urlencode("channel:read:redemptions"), "channel%3Aread%3Aredemptions");
        assert_eq!(
            urlencode("channel:read:redemptions moderator:read:chatters"),
            "channel%3Aread%3Aredemptions%20moderator%3Aread%3Achatters"
        );
        assert_eq!(urlencode("aA0-_.~"), "aA0-_.~");
        assert_eq!(urlencode("a+b&c=d"), "a%2Bb%26c%3Dd");
    }
}
