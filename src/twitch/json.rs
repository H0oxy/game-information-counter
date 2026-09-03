//! Мини-экстрактор полей JSON: «достань значение по ключу», а не парсер.
//!
//! `serde` в проекте нет намеренно - `web.rs` собирает свой JSON через
//! `format!`. Формы ответов Twitch фиксированы и документированы, полный
//! парсер под них избыточен.
//!
//! Всё возвращает `Option` и никогда не паникует: данные приходят из сети, а в
//! релизе `panic = "abort"` (тот же принцип, что у `parse_color` в config.rs и
//! `decode_attempt` в stats.rs - кривой вход даёт `None`, а не падение).
//!
//! **Строковые литералы пропускаются целиком.** Это не украшение: в том же
//! объекте едут ник зрителя и его `user_input`, то есть текст, который зритель
//! пишет сам. Без этого ник вида `","cost":999999,"x":"` притворился бы ключом.
//!
//! ponytail: ключ ищется на любом уровне вложенности - первый совпавший. Для
//! фиксированных форм Twitch этого хватает, а где вложенность важна, спуск
//! делается явно через `object_field`. Замена на `nanoserde` - правка внутри
//! этого файла, обвязка снаружи не меняется.

/// Содержимое строкового литерала (без кавычек) и индекс сразу за ним.
/// `start` должен указывать на открывающую кавычку.
fn read_string(b: &[u8], start: usize) -> Option<(&[u8], usize)> {
    if *b.get(start)? != b'"' {
        return None;
    }
    let from = start + 1;
    let mut i = from;
    while i < b.len() {
        match b[i] {
            // Экранированный символ проглатывается парой - иначе `\"` сойдёт за
            // конец строки.
            b'\\' => i += 2,
            b'"' => return Some((&b[from..i], i + 1)),
            _ => i += 1,
        }
    }
    None
}

/// Байтовая позиция значения, стоящего за `"key":`.
fn value_at(json: &str, key: &str) -> Option<usize> {
    let b = json.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'"' {
            i += 1;
            continue;
        }
        let (text, after) = read_string(b, i)?;
        // Ключ - это строка, за которой идёт двоеточие. Строка без него -
        // значение, и совпадение с искомым ключом там ничего не значит.
        let mut j = after;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b.get(j) == Some(&b':') && text == key.as_bytes() {
            let mut v = j + 1;
            while v < b.len() && b[v].is_ascii_whitespace() {
                v += 1;
            }
            return (v < b.len()).then_some(v);
        }
        i = after;
    }
    None
}

/// Индекс сразу за скобкой, парной к открывающей в `at`.
fn balanced_end(b: &[u8], at: usize, open: u8, close: u8) -> Option<usize> {
    if *b.get(at)? != open {
        return None;
    }
    let mut depth = 0usize;
    let mut i = at;
    while i < b.len() {
        if b[i] == b'"' {
            // Скобка внутри ника или user_input не должна сбивать счёт.
            let (_, after) = read_string(b, i)?;
            i = after;
            continue;
        }
        if b[i] == open {
            depth += 1;
        } else if b[i] == close {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}

fn hex4(raw: &[u8], at: usize) -> Option<u32> {
    let s = std::str::from_utf8(raw.get(at..at + 4)?).ok()?;
    u32::from_str_radix(s, 16).ok()
}

/// Разворачивает JSON-экранирование. `None` на любой некорректной
/// последовательности - лучше не отдать поле, чем отдать мусор.
fn unescape(raw: &[u8]) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] != b'\\' {
            let from = i;
            while i < raw.len() && raw[i] != b'\\' {
                i += 1;
            }
            // Кусок между экранированиями - валидный UTF-8 исходной строки.
            out.push_str(std::str::from_utf8(&raw[from..i]).ok()?);
            continue;
        }
        i += 1;
        let c = *raw.get(i)?;
        i += 1;
        match c {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'u' => {
                let cp = hex4(raw, i)?;
                i += 4;
                let ch = if (0xD800..0xDC00).contains(&cp) {
                    // Старший суррогат обязан идти в паре с младшим.
                    if raw.get(i) != Some(&b'\\') || raw.get(i + 1) != Some(&b'u') {
                        return None;
                    }
                    let lo = hex4(raw, i + 2)?;
                    if !(0xDC00..0xE000).contains(&lo) {
                        return None;
                    }
                    i += 6;
                    char::from_u32(0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00))?
                } else {
                    char::from_u32(cp)?
                };
                out.push(ch);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Позиция значения ключа **только на верхнем уровне** объекта.
///
/// Нужна там, где одноимённый ключ есть и во вложенном объекте: у погашения
/// награды `id` есть и у самого события, и у награды внутри него. Обычный
/// `value_at` берёт первый попавшийся, и на событии без своего `id` он вернул
/// бы id награды - а по нему потом «возвращаются» баллы совсем не за то.
fn value_at_top(json: &str, key: &str) -> Option<usize> {
    let b = json.as_bytes();
    let mut i = 0;
    // 0 - ещё не вошли в объект, 1 - внутри него, глубже - вложенное.
    let mut depth = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let (text, after) = read_string(b, i)?;
                let mut j = after;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                // Ключ считается нашим только на первом уровне вложенности.
                if depth == 1 && b.get(j) == Some(&b':') && text == key.as_bytes() {
                    let mut v = j + 1;
                    while v < b.len() && b[v].is_ascii_whitespace() {
                        v += 1;
                    }
                    return (v < b.len()).then_some(v);
                }
                i = after;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.checked_sub(1)?,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Строка по ключу верхнего уровня. См. `value_at_top`.
pub fn str_field_top(json: &str, key: &str) -> Option<String> {
    let b = json.as_bytes();
    let at = value_at_top(json, key)?;
    let (raw, _) = read_string(b, at)?;
    unescape(raw)
}

pub fn str_field(json: &str, key: &str) -> Option<String> {
    let b = json.as_bytes();
    let at = value_at(json, key)?;
    let (raw, _) = read_string(b, at)?;
    unescape(raw)
}

pub fn i64_field(json: &str, key: &str) -> Option<i64> {
    let b = json.as_bytes();
    let at = value_at(json, key)?;
    let mut end = at;
    if b.get(end) == Some(&b'-') {
        end += 1;
    }
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    json.get(at..end)?.parse().ok()
}

/// Булево по ключу. `true`/`false` идут без кавычек, поэтому `str_field` их
/// не видит вовсе.
pub fn bool_field(json: &str, key: &str) -> Option<bool> {
    let rest = json.get(value_at(json, key)?..)?;
    match rest {
        _ if rest.starts_with("true") => Some(true),
        _ if rest.starts_with("false") => Some(false),
        _ => None,
    }
}

pub fn u32_field(json: &str, key: &str) -> Option<u32> {
    i64_field(json, key)?.try_into().ok()
}

/// Объект-значение вместе со скобками. Нужен, чтобы спускаться по уровням явно
/// (`payload` -> `session` -> `id`), а не надеяться на поиск по всей строке:
/// `id` есть и у редима, и у награды внутри него.
pub fn object_field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let at = value_at(json, key)?;
    json.get(at..balanced_end(json.as_bytes(), at, b'{', b'}')?)
}

/// Элементы массива верхнего уровня, срезами. Для helix-ответов вида
/// `"data":[{...},{...}]`.
pub fn array_items<'a>(json: &'a str, key: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let b = json.as_bytes();
    let Some(at) = value_at(json, key) else { return out };
    let Some(end) = balanced_end(b, at, b'[', b']') else { return out };
    let last = end - 1;
    let mut i = at + 1;
    while i < last {
        match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' | b',' => i += 1,
            b'{' => {
                let Some(e) = balanced_end(b, i, b'{', b'}') else { return out };
                if let Some(s) = json.get(i..e) {
                    out.push(s);
                }
                i = e;
            }
            b'"' => {
                let Some((_, after)) = read_string(b, i) else { return out };
                if let Some(s) = json.get(i..after) {
                    out.push(s);
                }
                i = after;
            }
            _ => {
                let from = i;
                while i < last && b[i] != b',' {
                    i += 1;
                }
                if let Some(s) = json.get(from..i) {
                    out.push(s.trim());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ответ на запрос кода устройства - плоский объект.
    #[test]
    fn reads_device_code_response() {
        let j = r#"{"device_code":"abc123","expires_in":1800,"interval":5,
                    "user_code":"WXYZ7890","verification_uri":"https://www.twitch.tv/activate"}"#;
        assert_eq!(str_field(j, "device_code").as_deref(), Some("abc123"));
        assert_eq!(str_field(j, "user_code").as_deref(), Some("WXYZ7890"));
        assert_eq!(str_field(j, "verification_uri").as_deref(), Some("https://www.twitch.tv/activate"));
        assert_eq!(i64_field(j, "expires_in"), Some(1800));
        assert_eq!(i64_field(j, "interval"), Some(5));
        assert_eq!(str_field(j, "no_such_key"), None);
    }

    #[test]
    fn reads_token_response() {
        let j = r#"{"access_token":"tok","refresh_token":"ref","expires_in":14124,
                    "scope":["channel:read:redemptions"],"token_type":"bearer"}"#;
        assert_eq!(str_field(j, "access_token").as_deref(), Some("tok"));
        assert_eq!(str_field(j, "refresh_token").as_deref(), Some("ref"));
        assert_eq!(i64_field(j, "expires_in"), Some(14124));
    }

    /// Спуск по уровням: `payload.session.id`.
    #[test]
    fn reads_session_welcome() {
        let j = r#"{"metadata":{"message_type":"session_welcome"},
                    "payload":{"session":{"id":"sess-42","status":"connected",
                    "keepalive_timeout_seconds":10,"reconnect_url":null}}}"#;
        let meta = object_field(j, "metadata").unwrap();
        assert_eq!(str_field(meta, "message_type").as_deref(), Some("session_welcome"));
        let session = object_field(object_field(j, "payload").unwrap(), "session").unwrap();
        assert_eq!(str_field(session, "id").as_deref(), Some("sess-42"));
        assert_eq!(i64_field(session, "keepalive_timeout_seconds"), Some(10));
    }

    /// У редима и у награды внутри него ключ `id` одинаковый - спуск через
    /// `object_field` обязателен, иначе возьмётся первый попавшийся.
    #[test]
    fn reward_id_needs_an_explicit_descent() {
        let j = r#"{"payload":{"event":{"id":"redemption-1","user_name":"Nightrider",
                    "user_id":"777","user_input":"","status":"unfulfilled",
                    "reward":{"id":"reward-9","title":"Прыжок","cost":100}}}}"#;
        let event = object_field(object_field(j, "payload").unwrap(), "event").unwrap();
        assert_eq!(str_field(event, "id").as_deref(), Some("redemption-1"));
        assert_eq!(str_field(event, "user_name").as_deref(), Some("Nightrider"));
        let reward = object_field(event, "reward").unwrap();
        assert_eq!(str_field(reward, "id").as_deref(), Some("reward-9"));
        assert_eq!(str_field(reward, "title").as_deref(), Some("Прыжок"));
        assert_eq!(u32_field(reward, "cost"), Some(100));
    }

    /// Ключ верхнего уровня не должен подменяться одноимённым из вложенного
    /// объекта: у погашения `id` есть и у события, и у награды внутри него.
    #[test]
    fn top_level_lookup_ignores_nested() {
        let event = r#"{"user_name":"Nick","reward":{"id":"reward-9","title":"t"}}"#;
        assert_eq!(str_field_top(event, "id"), None, "своего id у события нет");
        assert_eq!(str_field(event, "id").as_deref(), Some("reward-9"), "обычный поиск найдёт вложенный");

        let with_own = r#"{"id":"redemption-1","reward":{"id":"reward-9"}}"#;
        assert_eq!(str_field_top(with_own, "id").as_deref(), Some("redemption-1"));
        // И вложенный по-прежнему достаётся явным спуском.
        let reward = object_field(with_own, "reward").unwrap();
        assert_eq!(str_field(reward, "id").as_deref(), Some("reward-9"));
    }

    /// Зритель пишет `user_input` сам. Попытка вложить туда пару ключ-значение
    /// не должна ничего подменять: строковый литерал пропускается целиком.
    #[test]
    fn viewer_text_cannot_forge_a_key() {
        let j = r#"{"event":{"user_input":"\",\"cost\":999999,\"x\":\"",
                    "reward":{"title":"Шаг","cost":50}}}"#;
        let reward = object_field(object_field(j, "event").unwrap(), "reward").unwrap();
        assert_eq!(u32_field(reward, "cost"), Some(50));
        // И сам подделанный текст читается как обычная строка.
        let event = object_field(j, "event").unwrap();
        assert_eq!(str_field(event, "user_input").as_deref(), Some(r#"","cost":999999,"x":""#));
    }

    /// Скобка внутри ника не должна рвать границу объекта.
    #[test]
    fn braces_inside_a_nickname_do_not_break_nesting() {
        let j = r#"{"event":{"user_name":"}{ troll }{","reward":{"cost":7}},"after":1}"#;
        let event = object_field(j, "event").unwrap();
        assert_eq!(str_field(event, "user_name").as_deref(), Some("}{ troll }{"));
        assert_eq!(u32_field(object_field(event, "reward").unwrap(), "cost"), Some(7));
        assert_eq!(i64_field(j, "after"), Some(1));
    }

    /// Строка-значение, совпадающая с искомым ключом, ключом не считается.
    #[test]
    fn a_value_that_looks_like_a_key_is_ignored() {
        let j = r#"{"title":"cost","cost":42}"#;
        assert_eq!(u32_field(j, "cost"), Some(42));
    }

    #[test]
    fn unescapes_what_twitch_sends() {
        let j = r#"{"a":"quote \" backslash \\ newline \n tab \t","b":"Маргит","c":"💀"}"#;
        assert_eq!(str_field(j, "a").as_deref(), Some("quote \" backslash \\ newline \n tab \t"));
        assert_eq!(str_field(j, "b").as_deref(), Some("Маргит"));
        assert_eq!(str_field(j, "c").as_deref(), Some("💀"));
    }

    #[test]
    fn reads_arrays_of_objects() {
        let j = r#"{"data":[{"user_login":"alpha"},{"user_login":"beta"}],"total":2}"#;
        let items = array_items(j, "data");
        assert_eq!(items.len(), 2);
        assert_eq!(str_field(items[0], "user_login").as_deref(), Some("alpha"));
        assert_eq!(str_field(items[1], "user_login").as_deref(), Some("beta"));
        assert_eq!(i64_field(j, "total"), Some(2));
        assert!(array_items(j, "missing").is_empty());
    }

    #[test]
    fn negative_numbers() {
        let j = r#"{"n":-17}"#;
        assert_eq!(i64_field(j, "n"), Some(-17));
        assert_eq!(u32_field(j, "n"), None, "отрицательное не лезет в u32");
    }

    /// Оборванный ответ (сеть отвалилась на середине) не должен паниковать -
    /// в релизе panic = "abort", это падение игры.
    #[test]
    fn truncated_and_broken_input_returns_none() {
        for j in [
            "",
            "{",
            r#"{"access_token""#,
            r#"{"access_token":"#,
            r#"{"access_token":"abc"#,
            r#"{"a":"broken\"#,
            r#"{"a":"\u00"#,
            r#"{"a":"\q"}"#,
            r#"{"payload":{"session":{"#,
        ] {
            assert_eq!(str_field(j, "access_token"), None, "{j}");
            assert_eq!(str_field(j, "a"), None, "{j}");
            assert_eq!(i64_field(j, "a"), None, "{j}");
            assert_eq!(object_field(j, "payload"), None, "{j}");
            assert!(array_items(j, "data").is_empty(), "{j}");
        }
    }
}
