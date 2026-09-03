//! Секреты на диске: Client ID и токены Twitch лежат зашифрованными.
//!
//! Именно шифрование, а не хэш: значение мод не сверяет, а использует - из
//! хэша Client ID обратно не достать, для этой задачи он не годится вовсе.
//!
//! Ключа у нас нет и хранить его негде: DPAPI (`CryptProtectData`) шифрует на
//! учётной записи Windows. Файл, утащенный на другую машину или прочитанный
//! другим пользователем, не расшифруется, а нам не приходится придумывать,
//! куда спрятать ключ - иначе он лежал бы в той же DLL рядом с данными.
//!
//! Формат в файле: `enc:` + hex. Открытый текст остаётся читаемым (человек
//! вставляет ID руками), и мод перешифровывает его при первом же сохранении.

use std::ffi::c_void;

/// Метка «здесь шифртекст». Всё без неё считается вставленным вручную.
const PREFIX: &str = "enc:";

#[repr(C)]
struct DataBlob {
    cb: u32,
    pb: *mut u8,
}

#[link(name = "crypt32")]
extern "system" {
    fn CryptProtectData(
        input: *const DataBlob,
        description: *const u16,
        entropy: *const DataBlob,
        reserved: *mut c_void,
        prompt: *mut c_void,
        flags: u32,
        output: *mut DataBlob,
    ) -> i32;
    fn CryptUnprotectData(
        input: *const DataBlob,
        description: *mut *mut u16,
        entropy: *const DataBlob,
        reserved: *mut c_void,
        prompt: *mut c_void,
        flags: u32,
        output: *mut DataBlob,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
}

/// Без интерфейса пользователя: мы в игре, и диалог DPAPI поверх неё был бы
/// невидимым окном, которого никто не закроет.
const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Общая обвязка над обеими функциями DPAPI: они отличаются только вызовом.
fn dpapi(data: &[u8], protect: bool) -> Option<Vec<u8>> {
    // Пустой вход DPAPI не принимает, а нам он и не нужен.
    if data.is_empty() {
        return None;
    }
    let input = DataBlob { cb: data.len() as u32, pb: data.as_ptr() as *mut u8 };
    let mut out = DataBlob { cb: 0, pb: std::ptr::null_mut() };
    let ok = unsafe {
        if protect {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
    };
    if ok == 0 || out.pb.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(out.pb, out.cb as usize) }.to_vec();
    // Буфер выдала система - освобождать обязаны мы.
    unsafe { LocalFree(out.pb.cast()) };
    Some(bytes)
}

/// Значение в том виде, в каком оно ложится в файл.
///
/// Не получилось зашифровать (DPAPI недоступен, профиль пользователя не
/// загружен) - возвращаем как есть: потерять Client ID хуже, чем сохранить его
/// открытым, а мод в этом случае просто работает как раньше.
pub fn protect(plain: &str) -> String {
    let plain = plain.trim();
    if plain.is_empty() || plain.starts_with(PREFIX) {
        return plain.to_string();
    }
    match dpapi(plain.as_bytes(), true) {
        Some(bytes) => format!("{PREFIX}{}", to_hex(&bytes)),
        None => plain.to_string(),
    }
}

/// Обратно: `enc:...` расшифровывается, всё остальное отдаётся как есть -
/// значит человек вписал его руками.
///
/// Расшифровать не вышло (файл с чужой машины) - пусто, а не мусор: мод скажет
/// «не введён Client ID», и это ближе к правде, чем биться в Twitch с
/// нечитаемой строкой.
pub fn reveal(stored: &str) -> String {
    let stored = stored.trim();
    let Some(hex) = stored.strip_prefix(PREFIX) else {
        return stored.to_string();
    };
    from_hex(hex)
        .and_then(|bytes| dpapi(&bytes, false))
        .and_then(|plain| String::from_utf8(plain).ok())
        .unwrap_or_default()
}

/// Лежит ли значение в файле уже зашифрованным. По нему `Config::load`
/// понимает, что вставленный руками секрет пора перешифровать.
pub fn is_protected(stored: &str) -> bool {
    stored.trim().starts_with(PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Круг «зашифровать - расшифровать» на живом DPAPI: тесты идут под тем же
    /// пользователем Windows, что и игра.
    #[test]
    fn round_trips_through_dpapi() {
        let stored = protect("abcdef1234567890");
        assert!(is_protected(&stored), "секрет обязан уехать в файл зашифрованным");
        assert!(!stored.contains("abcdef1234567890"), "открытого текста в файле не остаётся");
        assert_eq!(reveal(&stored), "abcdef1234567890");
    }

    /// Вставленное руками значение читается как есть, а пустое остаётся пустым.
    #[test]
    fn plain_text_passes_through() {
        assert_eq!(reveal("abcdef1234567890"), "abcdef1234567890");
        assert_eq!(protect(""), "");
        assert_eq!(reveal(""), "");
        assert!(!is_protected("abcdef"));
    }

    /// Дважды шифровать нельзя: `protect` зовётся и при сохранении из окна
    /// настроек, и при перешифровке при загрузке.
    #[test]
    fn protecting_twice_changes_nothing() {
        let once = protect("token-value");
        assert_eq!(protect(&once), once);
    }

    /// Файл с чужой машины расшифровать нечем - это пусто, а не мусор.
    #[test]
    fn foreign_ciphertext_reveals_to_nothing() {
        assert_eq!(reveal("enc:00ff00ff"), "");
        assert_eq!(reveal("enc:не-хекс"), "");
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(from_hex(&to_hex(&[0, 1, 254, 255])), Some(vec![0, 1, 254, 255]));
        assert_eq!(from_hex("abc"), None, "нечётная длина - не hex");
    }
}
