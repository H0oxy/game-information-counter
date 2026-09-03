//! Минимальный HTTPS-клиент: один запрос - одно соединение.
//!
//! Зеркало `web.rs`, только клиентская сторона: там свой HTTP-сервер на
//! `std::net` без зависимостей, здесь свой клиент поверх TLS, который и так уже
//! оплачен (rustls приезжает с `tokio-tungstenite` ради `wss://`). Отдельный
//! HTTP-клиент (`reqwest`, `ureq`) ради нескольких запросов к двум известным
//! хостам - самая тяжёлая из возможных плат.
//!
//! `Connection: close` на каждом запросе, как и у сервера в `web.rs`: тело
//! читается до EOF, и длину можно не разбирать вовсе. Разобрать приходится
//! только `chunked` - его Twitch вправе прислать независимо от закрытия.
//!
//! Ничего не паникует: всё возвращает `Option`. В релизе `panic = "abort"`,
//! а данные тут сетевые.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

/// Ответ целиком. Тело - строка: у Twitch это всегда JSON в UTF-8.
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Один TLS-конфиг на процесс: сборка хранилища корней стоит заметно дороже
/// самого запроса, а меняться ему не с чего.
fn connector() -> Option<&'static TlsConnector> {
    static TLS: OnceLock<Option<TlsConnector>> = OnceLock::new();
    TLS.get_or_init(|| {
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        // Провайдер задаётся явно, а не через `ClientConfig::builder()`: тот
        // берёт глобальный default и паникует, если его не установили или если
        // в графе оказалось два провайдера. Паника здесь - падение игры.
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .ok()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Some(TlsConnector::from(Arc::new(config)))
    })
    .as_ref()
}

/// Потолок на весь запрос: соединение, рукопожатие и чтение вместе.
///
/// Без него «чёрная дыра» в сети (пакеты уходят и не возвращаются) вешает
/// сетевой поток навсегда: у TCP свои многоминутные таймауты, а чтение до EOF
/// не заканчивается вообще, если сервер не закрывает соединение. Снаружи это
/// выглядело бы как вечное «подключаемся...» без единой попытки повтора.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// `POST`/`GET` на `https://{host}{path}`. `headers` - дополнительные строки
/// вида `("Authorization", "Bearer ...")`.
pub async fn request(
    host: &str,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Option<&str>,
) -> Option<Response> {
    tokio::time::timeout(REQUEST_TIMEOUT, request_inner(host, method, path, headers, body))
        .await
        .ok()?
}

async fn request_inner(
    host: &str,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Option<&str>,
) -> Option<Response> {
    let tcp = TcpStream::connect((host, 443)).await.ok()?;
    let name = ServerName::try_from(host.to_string()).ok()?;
    let mut tls = connector()?.connect(name, tcp).await.ok()?;

    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n"
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    tls.write_all(head.as_bytes()).await.ok()?;
    if let Some(body) = body {
        tls.write_all(body.as_bytes()).await.ok()?;
    }
    tls.flush().await.ok()?;

    // Предел на ответ: сервер, который льёт байты и не заканчивает, не должен
    // съесть память. Самый большой ожидаемый ответ - список чата, он
    // укладывается с огромным запасом.
    const MAX_BODY: usize = 1024 * 1024;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match tls.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() >= MAX_BODY {
                    break;
                }
            }
            // Обрыв TLS без `close_notify` - для `Connection: close` обычное
            // дело, и rustls честно считает это ошибкой. Но ответ к этому
            // моменту уже прочитан целиком, и выбрасывать его нельзя:
            // `read_to_end` тут возвращал ошибку на весь запрос, и снаружи это
            // выглядело как «нет связи» при живой сети.
            Err(_) => break,
        }
    }

    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Option<Response> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let body = &raw[split + 4..];

    let mut lines = head.split("\r\n");
    // "HTTP/1.1 200 OK" - код вторым словом.
    let status = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;

    let chunked = lines.any(|l| {
        let (name, value) = l.split_once(':').unwrap_or(("", ""));
        name.eq_ignore_ascii_case("transfer-encoding") && value.to_ascii_lowercase().contains("chunked")
    });

    let body = if chunked { dechunk(body)? } else { body.to_vec() };
    Some(Response {
        status,
        // Тело от Twitch - JSON в UTF-8; битую последовательность заменяем, а не
        // теряем весь ответ.
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Склеивает `chunked`-тело. Формат: длина в hex, CRLF, данные, CRLF, ...,
/// нулевая длина в конце.
fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n")?;
        // После длины может идти `;расширение` - оно нас не касается.
        let head = std::str::from_utf8(&b[..eol]).ok()?;
        let size = usize::from_str_radix(head.split(';').next()?.trim(), 16).ok()?;
        if size == 0 {
            return Some(out);
        }
        let from = eol + 2;
        let to = from.checked_add(size)?;
        out.extend_from_slice(b.get(from..to)?);
        // Пропускаем CRLF после данных.
        b = b.get(to + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"a\":1}";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "{\"a\":1}");
        assert!(r.ok());
    }

    #[test]
    fn unauthorized_is_not_ok() {
        let raw = b"HTTP/1.1 401 Unauthorized\r\n\r\n{\"message\":\"invalid\"}";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 401);
        assert!(!r.ok());
    }

    #[test]
    fn joins_chunked_body() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n3\r\n1}\x00\r\n0\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.body, "{\"a\":1}\u{0}");
    }

    /// Обрыв на середине - пустой ответ, а не паника.
    #[test]
    fn truncated_input_is_none() {
        assert!(parse_response(b"").is_none());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_none());
        assert!(parse_response(b"garbage\r\n\r\n").is_none());
        // Заголовок обещает chunked, а тело оборвано.
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab").is_none());
    }
}
