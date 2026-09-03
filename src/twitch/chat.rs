//! Зрители из чата - анонимно, по одному имени канала.
//!
//! Twitch пускает в чат без всякой авторизации: логин вида `justinfan<цифры>`
//! и любой пароль. Ни приложения, ни токена, ни прав не нужно - достаточно
//! знать канал.
//!
//! Именно поэтому никнеймы над врагами взяты отсюда, а не из
//! `helix/chat/chatters`: тот требует права `moderator:read:chatters`, а права
//! зашиты в уже выданный токен - включить их постфактум можно было бы только
//! повторной авторизацией. Здесь этого нет вовсе.
//!
//! Что видно анонимно: список при входе (RPL_NAMREPLY), заходы и выходы
//! (`JOIN`/`PART`) и все, кто пишет. Для подписей над врагами этого более чем
//! достаточно.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use super::Event;

const CHAT_WS: &str = "wss://irc-ws.chat.twitch.tv:443";

/// Пауза перед переподключением. Чат - не критичная часть: если он отвалится,
/// пропадут только подписи над врагами.
const RETRY: Duration = Duration::from_secs(15);

/// Сколько ждём молчащий сокет. Twitch сам шлёт PING примерно раз в 5 минут.
const READ_TIMEOUT: Duration = Duration::from_secs(360);

/// Шаг ожидания сокета. Полный таймаут набирается из таких шагов - иначе смену
/// канала в настройках пришлось бы ждать до следующего сообщения в чате,
/// то есть на тихом канале минутами.
const READ_STEP: Duration = Duration::from_secs(2);

/// Поколение чат-потока. Растёт на каждый запуск; поток, чьё поколение
/// устарело, выходит сам.
///
/// Смена канала обязана обнулять список: раньше `chat_started` поднимал поток
/// один раз за сессию, и после вставки другой ссылки в списке продолжали
/// висеть зрители прошлого канала - их имена и раздавались врагам (жалоба).
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Останавливает текущий чат-поток, не поднимая нового: канал стёрли.
pub fn stop() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Не чаще этого отдаём наружу обновлённый список.
const PUBLISH_EVERY: Duration = Duration::from_secs(3);

/// Имя канала из того, что ввёл пользователь.
///
/// Принимает и голое имя, и ссылку целиком - на дашборде Twitch проще
/// скопировать адрес из строки браузера, чем выцарапывать логин.
pub fn channel_name(input: &str) -> Option<String> {
    let s = input.trim().trim_end_matches('/');
    // Отрезаем схему и хост, если это ссылка.
    let s = s.rsplit('/').next().unwrap_or(s);
    // Хвост вида `?tt_content=...` встречается в скопированных ссылках.
    let s = s.split(['?', '#']).next().unwrap_or(s);
    let s = s.trim().trim_start_matches('@').to_ascii_lowercase();
    // Логины Twitch - только буквы, цифры и подчёркивание.
    let ok = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    ok.then_some(s)
}

/// Что пришло из чата.
#[derive(PartialEq, Debug)]
enum Line {
    /// Список тех, кто уже в канале (приходит при входе).
    Names(Vec<String>),
    Joined(String),
    Left(String),
    /// Кто-то написал сообщение - он точно живой и активный. Второе поле -
    /// сам текст: он идёт в подпись под ником над врагом («последнее слово»).
    Spoke(String, String),
    /// Сервер просит ответить, иначе отключит.
    Ping(String),
    Other,
}

/// Разбор строки IRC. Отдельно от сети, поэтому проверяется тестами целиком.
fn parse_line(line: &str) -> Line {
    let line = line.trim_end_matches(['\r', '\n']);
    if let Some(token) = line.strip_prefix("PING") {
        return Line::Ping(token.trim().trim_start_matches(':').to_string());
    }
    if !line.starts_with(':') {
        return Line::Other;
    }
    // `:nick!user@host COMMAND params...`
    let (prefix, rest) = line[1..].split_once(' ').unwrap_or((&line[1..], ""));
    let nick = prefix.split(['!', '@']).next().unwrap_or_default().to_string();
    let mut parts = rest.split_whitespace();
    let command = parts.next().unwrap_or_default();

    match command {
        "JOIN" if !nick.is_empty() => Line::Joined(nick),
        "PART" if !nick.is_empty() => Line::Left(nick),
        "PRIVMSG" if !nick.is_empty() => {
            // `PRIVMSG #channel :текст` - текст всегда после первого " :".
            let said = rest.split_once(" :").map(|(_, t)| t).unwrap_or_default();
            Line::Spoke(nick, said.to_string())
        }
        // `353 justinfan = #channel :nick1 nick2 nick3`
        "353" => match rest.split_once(':') {
            Some((_, names)) => Line::Names(names.split_whitespace().map(str::to_string).collect()),
            None => Line::Other,
        },
        _ => Line::Other,
    }
}

/// Последние реплики зрителей: ник в нижнем регистре и текст.
///
/// Отдельная карта, а не поле в списке зрителей: список ходит через все слои
/// (`Event::ChattersUpdated`, `NicknameAssigner`, `Shared`), а реплика нужна
/// ровно в одном месте - в подписи над врагом. `Vec`, а не `HashMap`:
/// `Vec::new` const, потолок мал, а искать в нём приходится максимум восемь
/// раз за кадр - по числу тегов, которые вообще показывает игра.
static SAID: std::sync::Mutex<Vec<(String, String, Option<std::time::Instant>)>> =
    std::sync::Mutex::new(Vec::new());

/// Сколько реплик держим. Больше не нужно: подписать можно только тех, кто
/// прямо сейчас на экране.
const SAID_KEPT: usize = 200;

/// Максимум букв в реплике. Длиннее - это уже не подпись над врагом, а стена
/// текста поперёк экрана.
const SAID_LEN: usize = 48;

/// Чистка чужого текста перед показом на экране стримера.
///
/// Два ограничения в одном месте:
/// - **глифы.** Атлас печётся с `FontGlyphRanges::cyrillic()` (Latin-1 плюс
///   кириллица), и всё остальное - эмодзи, CJK, стрелки - ImGui нарисует
///   знаком вопроса. Выбрасываем молча: ряд «?????» хуже текста без смайлика.
/// - **длина и переводы строк**: подпись рисуется одной строкой.
fn clean_said(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for ch in text.chars() {
        let keep = ch == ' '
            || ('\u{21}'..='\u{FF}').contains(&ch)
            || ('\u{400}'..='\u{4FF}').contains(&ch);
        if !keep {
            continue;
        }
        if ch == ' ' {
            // Схлопываем пробелы: подряд идущие читаются разрывом строки.
            if space || out.is_empty() {
                continue;
            }
            space = true;
        } else {
            space = false;
        }
        out.push(ch);
        if out.chars().count() >= SAID_LEN {
            break;
        }
    }
    out.trim_end().to_string()
}

/// Запоминает, что сказал зритель. Пустое (одни эмодзи) не запоминаем - пусть
/// остаётся прошлая реплика.
fn remember(nick: &str, text: &str) {
    let said = clean_said(text);
    if said.is_empty() {
        return;
    }
    let key = nick.to_lowercase();
    let Ok(mut list) = SAID.lock() else {
        return;
    };
    match list.iter_mut().find(|(n, ..)| *n == key) {
        Some(slot) => {
            slot.1 = said;
            // Новая реплика начинает свой срок заново: зритель написал ещё
            // раз - значит над врагом снова должно появиться.
            slot.2 = None;
        }
        None => {
            if list.len() >= SAID_KEPT {
                list.remove(0);
            }
            list.push((key, said, None));
        }
    }
}

/// Что зритель написал последним и сколько секунд эта реплика уже на экране.
///
/// **Срок идёт от ПЕРВОГО показа, а не от момента отправки в чат.** Написали в
/// чате в одну минуту, а ник достаётся врагу когда он попал в кадр - то есть
/// почти всегда позже. По времени отправки реплика оказывалась мёртвой ещё до
/// того, как её кто-то увидел (жалоба 2026-08-21: «перестал показывать»).
///
/// Отсюда и `&mut`: первый запрос запускает часы.
pub fn said(nick: &str) -> Option<(String, f32)> {
    let key = nick.to_lowercase();
    let mut list = SAID.lock().ok()?;
    let slot = list.iter_mut().find(|(n, ..)| *n == key)?;
    let at = *slot.2.get_or_insert_with(std::time::Instant::now);
    Some((slot.1.clone(), at.elapsed().as_secs_f32()))
}

/// Сколько зрителей держим.
///
/// Было 300, и это упиралось в потолок раньше, чем в реальность: на канале с
/// 4000 онлайн список набирался до 300 и переставал расти (жалоба
/// 2026-08-19). 2000 - с запасом относительно того, сколько людей вообще
/// пишет в чат за сессию, а стоит это единицы сотен килобайт.
///
/// **Потолок роста задаёт не этот кап, а сам Twitch**: анонимному клиенту он
/// шлёт `JOIN`/`PART` только на небольших каналах, а на людных - нет вовсе.
/// Оттуда и «набирается медленно»: видно ровно тех, кто ПИШЕТ, плюс список из
/// `353` при входе. Луркеров анонимно не получить ничем, кроме
/// `helix/chat/chatters`, а он требует права `moderator:read:chatters` -
/// ровно того, от чего ушли (см. шапку модуля).
pub const MAX_TRACKED: usize = 2000;

/// Кто сейчас в чате. Наружу отдаётся списком: потребителю нужен не поток
/// событий, а «кого можно назначить врагу прямо сейчас».
///
/// Вытеснение по давности активности - при переполнении уходит тот, кто дольше
/// всех молчал. Активные зрители держатся в списке сами собой.
#[derive(Default)]
struct Roster {
    /// Ник, который не берём в список, - наш собственный.
    ignore: String,
    people: Vec<String>,
    /// Ник в нижнем регистре -> место в `people`. Без него каждое сообщение
    /// стоило бы линейного поиска по всему списку.
    index: std::collections::HashMap<String, usize>,
    /// Когда зрителя видели в последний раз, в тиках. Параллелен `people`.
    ///
    /// Раньше активность вела `VecDeque` ников, и каждое сообщение стоило
    /// поиска в ней плюс сдвига (`position` + `remove`) - на списке в 2000 это
    /// уже заметно. Тик - это одна запись в вектор по индексу.
    seen: Vec<u64>,
    /// Монотонный счётчик: сравниваются только между собой.
    tick: u64,
}

impl Roster {
    fn add(&mut self, nick: String) -> bool {
        if nick.is_empty() {
            return false;
        }
        let key = nick.to_lowercase();
        if key == self.ignore {
            return false;
        }
        self.tick += 1;
        if let Some(&pos) = self.index.get(&key) {
            // Уже знаем - только освежаем активность, состав не меняется.
            self.seen[pos] = self.tick;
            return false;
        }
        self.index.insert(key, self.people.len());
        self.people.push(nick);
        self.seen.push(self.tick);
        if self.people.len() > MAX_TRACKED {
            self.evict_stalest();
        }
        true
    }

    /// Выбрасывает того, кого дольше всех не было слышно. Линейный поиск, но
    /// только в момент переполнения, а не на каждое сообщение.
    fn evict_stalest(&mut self) {
        let Some(pos) = (0..self.seen.len()).min_by_key(|&i| self.seen[i]) else {
            return;
        };
        let key = self.people[pos].to_lowercase();
        self.remove_key(&key);
    }

    fn remove(&mut self, nick: &str) -> bool {
        let key = nick.to_lowercase();
        self.remove_key(&key)
    }

    /// `swap_remove` вместо `retain`: список бывает длинным, а порядок в нём
    /// ничего не значит - имена раздаются по кругу.
    fn remove_key(&mut self, key: &str) -> bool {
        let Some(pos) = self.index.remove(key) else {
            return false;
        };
        self.people.swap_remove(pos);
        self.seen.swap_remove(pos);
        // Переехавшему на освободившееся место надо поправить индекс.
        if let Some(moved) = self.people.get(pos) {
            self.index.insert(moved.to_lowercase(), pos);
        }
        true
    }

    fn apply(&mut self, line: Line) -> bool {
        match line {
            Line::Names(list) => {
                let mut changed = false;
                for nick in list {
                    changed |= self.add(nick);
                }
                changed
            }
            Line::Spoke(nick, said) => {
                remember(&nick, &said);
                self.add(nick)
            }
            Line::Joined(nick) => self.add(nick),
            Line::Left(nick) => self.remove(&nick),
            Line::Ping(_) | Line::Other => false,
        }
    }
}

/// Поднимает чат в своём потоке. Работает независимо от авторизации: канал
/// может быть подключён, даже когда Client ID не введён вовсе.
pub fn spawn(channel: String, tx: Sender<Event>) {
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    // Смена канала обнуляет и реплики: зрители там другие, а «последнее слово»
    // от прежнего канала висело бы над врагом как своё.
    if let Ok(mut said) = SAID.lock() {
        said.clear();
    }
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            return;
        };
        rt.block_on(async move {
            while current(generation) {
                let _ = session(&channel, &tx, generation).await;
                // Соединение кончилось - список зрителей больше не актуален.
                // Но только если канал всё ещё наш: иначе мы стёрли бы список,
                // который уже собирает поток нового канала.
                if !current(generation) {
                    return;
                }
                let _ = tx.send(Event::ChattersUpdated { list: Vec::new(), helix: false });
                tokio::time::sleep(RETRY).await;
            }
        });
    });
}

/// Наше ли ещё поколение. `false` - канал сменили, поток пора закрывать.
fn current(generation: u64) -> bool {
    GENERATION.load(Ordering::Relaxed) == generation
}

async fn session(channel: &str, tx: &Sender<Event>, generation: u64) -> Option<()> {
    let connect = tokio::time::timeout(Duration::from_secs(20), tokio_tungstenite::connect_async(CHAT_WS)).await;
    let Ok(Ok((mut ws, _))) = connect else {
        return None;
    };

    // Анонимный вход. Пароль не проверяется, ник обязан начинаться с
    // `justinfan` - так Twitch отличает read-only гостя.
    let nick = format!("justinfan{}", 10_000 + (channel.len() as u32 * 7919) % 80_000);
    ws.send(Message::Text("CAP REQ :twitch.tv/membership".into())).await.ok()?;
    ws.send(Message::Text("PASS SCHMOOPIIE".into())).await.ok()?;
    ws.send(Message::Text(format!("NICK {nick}").into())).await.ok()?;
    ws.send(Message::Text(format!("JOIN #{channel}").into())).await.ok()?;

    let mut roster = Roster::default();
    // Собственный ник в список зрителей попадать не должен: Twitch присылает
    // нас и в списке при входе (353), и своим `JOIN`. Живьём это выглядело как
    // «justinfan81271» над врагом (найдено пробником 2026-08-19).
    roster.ignore = nick.to_lowercase();
    // Первую публикацию не задерживаем: список при входе приходит сразу.
    // `checked_sub`, а не `-`: вычитание из свежего `Instant` паникует, если
    // машина только что загрузилась, а `panic = "abort"` - это краш игры.
    let now = std::time::Instant::now();
    let mut published = now.checked_sub(PUBLISH_EVERY).unwrap_or(now);
    let mut last_message = std::time::Instant::now();
    loop {
        // Ждём короткими шагами, чтобы замечать смену канала, а не висеть до
        // следующего сообщения в чате.
        let next = match tokio::time::timeout(READ_STEP, ws.next()).await {
            Ok(next) => next,
            Err(_) => {
                if !current(generation) || last_message.elapsed() >= READ_TIMEOUT {
                    return None;
                }
                continue;
            }
        };
        last_message = std::time::Instant::now();
        let text = match next? {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Ping(p)) => {
                ws.send(Message::Pong(p)).await.ok()?;
                continue;
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            _ => continue,
        };

        let mut changed = false;
        // В одном кадре приезжает сразу несколько строк.
        for line in text.split('\n').filter(|l| !l.trim().is_empty()) {
            let parsed = parse_line(line);
            if let Line::Ping(token) = &parsed {
                // Не ответить на PING - быть отключённым через минуту.
                ws.send(Message::Text(format!("PONG :{token}").into())).await.ok()?;
                continue;
            }
            changed |= roster.apply(parsed);
        }

        // Публикуем не на каждое сообщение, а раз в несколько секунд: на людном
        // канале строки идут пачками, и копировать весь список под каждую
        // значит гонять его тысячи раз в минуту впустую. Подписи над врагами от
        // секундной задержки не страдают.
        // `current` ещё раз: канал могли сменить секунду назад, и старый
        // список не должен приезжать поверх нового.
        if changed && published.elapsed() >= PUBLISH_EVERY && current(generation) {
            published = std::time::Instant::now();
            if tx.send(Event::ChattersUpdated { list: roster.people.clone(), helix: false }).is_err() {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_name_or_link() {
        assert_eq!(channel_name("shroud").as_deref(), Some("shroud"));
        assert_eq!(channel_name("  Shroud ").as_deref(), Some("shroud"));
        assert_eq!(channel_name("@shroud").as_deref(), Some("shroud"));
        assert_eq!(channel_name("https://www.twitch.tv/shroud").as_deref(), Some("shroud"));
        assert_eq!(channel_name("twitch.tv/shroud/").as_deref(), Some("shroud"));
        assert_eq!(channel_name("https://twitch.tv/shroud?tt_content=x").as_deref(), Some("shroud"));
        assert_eq!(channel_name("my_channel_1").as_deref(), Some("my_channel_1"));
    }

    /// Кривой ввод не должен превращаться в мусорный JOIN.
    #[test]
    fn rejects_junk() {
        assert_eq!(channel_name(""), None);
        assert_eq!(channel_name("   "), None);
        assert_eq!(channel_name("два слова"), None);
        assert_eq!(channel_name("bad name"), None);
        assert_eq!(channel_name("bad!name"), None);
        // `#` режется как якорь ссылки - это нормально для адреса, а не мусор.
        assert_eq!(channel_name("https://twitch.tv/shroud#about").as_deref(), Some("shroud"));
    }

    #[test]
    fn viewer_text_is_cleaned_before_it_reaches_the_screen() {
        // Атлас знает только Latin-1 и кириллицу: всё прочее ImGui нарисует
        // знаком вопроса, поэтому выбрасываем молча.
        assert_eq!(clean_said("привет 🔥 stream"), "привет stream");
        assert_eq!(clean_said("  двойные   пробелы "), "двойные пробелы");
        assert_eq!(clean_said("🔥🔥🔥"), "", "одни эмодзи - нечего показывать");
        assert!(clean_said(&"я".repeat(200)).chars().count() <= SAID_LEN);
        // Перевод строки в подписи разорвал бы её пополам.
        assert_eq!(clean_said("две\nстроки"), "двестроки");
    }

    #[test]
    fn last_word_is_remembered_per_viewer() {
        remember("Alpha", "первое");
        remember("ALPHA", "второе");
        let (text, age) = said("alpha").expect("реплика должна найтись");
        assert_eq!(text, "второе");
        assert!(age < 1.0, "часы начинаются с первого показа, а не раньше");
        assert!(said("нет-такого").is_none());

        // Срок идёт от показа: подкручиваем момент первого показа в прошлое -
        // возраст обязан вырасти вместе с ним.
        if let Ok(mut list) = SAID.lock() {
            if let Some(slot) = list.iter_mut().find(|(n, ..)| n == "alpha") {
                slot.2 = Some(std::time::Instant::now() - std::time::Duration::from_secs(30));
            }
        }
        assert!(said("alpha").expect("реплика на месте").1 > 29.0);

        // Новая реплика начинает срок заново.
        remember("alpha", "третье");
        assert!(said("alpha").expect("реплика на месте").1 < 1.0);
    }

    #[test]
    fn parses_membership_and_messages() {
        assert_eq!(
            parse_line(":justinfan1.tmi.twitch.tv 353 justinfan1 = #ch :alpha beta gamma"),
            Line::Names(vec!["alpha".into(), "beta".into(), "gamma".into()])
        );
        assert_eq!(parse_line(":alpha!alpha@alpha.tmi.twitch.tv JOIN #ch"), Line::Joined("alpha".into()));
        assert_eq!(parse_line(":beta!beta@beta.tmi.twitch.tv PART #ch"), Line::Left("beta".into()));
        assert_eq!(
            parse_line(":gamma!gamma@gamma.tmi.twitch.tv PRIVMSG #ch :привет"),
            Line::Spoke("gamma".into(), "привет".into())
        );
        assert_eq!(parse_line("PING :tmi.twitch.tv"), Line::Ping("tmi.twitch.tv".into()));
        assert_eq!(parse_line(":tmi.twitch.tv 001 justinfan1 :Welcome"), Line::Other);
        assert_eq!(parse_line(""), Line::Other);
    }

    #[test]
    fn roster_tracks_who_is_here() {
        let mut r = Roster::default();
        assert!(r.apply(Line::Names(vec!["alpha".into(), "beta".into()])));
        assert_eq!(r.people.len(), 2);

        // Тот же ник вторым заходом ничего не меняет.
        assert!(!r.apply(Line::Joined("alpha".into())));
        assert!(!r.apply(Line::Spoke("ALPHA".into(), String::new())), "регистр не должен плодить дубли");
        assert_eq!(r.people.len(), 2);

        assert!(r.apply(Line::Joined("gamma".into())));
        assert!(r.apply(Line::Left("beta".into())));
        assert_eq!(r.people, vec!["alpha".to_string(), "gamma".to_string()]);

        // Ушедшего второй раз убирать нечего.
        assert!(!r.apply(Line::Left("beta".into())));
    }

    /// Канал на тысячи зрителей не должен превращаться в список на тысячи
    /// строк: для подписей хватает сотен, а стоимость обязана быть постоянной.
    #[test]
    fn roster_is_capped() {
        let mut r = Roster::default();
        for i in 0..(MAX_TRACKED + 50) {
            r.apply(Line::Joined(format!("viewer{i}")));
        }
        assert_eq!(r.people.len(), MAX_TRACKED);
        assert_eq!(r.index.len(), MAX_TRACKED);
        // Вытесняются самые давние, свежие остаются.
        let newest = format!("viewer{}", MAX_TRACKED + 49);
        assert!(r.people.contains(&newest), "свежий вытеснен: {newest}");
        assert!(!r.people.iter().any(|n| n == "viewer0"), "самый давний должен был уйти");
    }

    /// Активный зритель не должен вытесняться: каждое его сообщение освежает
    /// место в очереди.
    #[test]
    fn active_viewer_survives_eviction() {
        let mut r = Roster::default();
        r.apply(Line::Joined("regular".into()));
        for i in 0..MAX_TRACKED {
            r.apply(Line::Joined(format!("v{i}")));
            r.apply(Line::Spoke("regular".into(), String::new()));
        }
        assert!(r.people.iter().any(|n| n == "regular"), "писавший остаётся в списке");
    }

    /// Удаление из середины не должно ломать индекс - на нём держится весь
    /// быстрый поиск.
    #[test]
    fn index_survives_removal() {
        let mut r = Roster::default();
        for n in ["a", "b", "c", "d"] {
            r.apply(Line::Joined(n.into()));
        }
        assert!(r.apply(Line::Left("b".into())));
        assert_eq!(r.people.len(), 3);
        // Повторный заход после ухода снова считается новым.
        assert!(r.apply(Line::Joined("b".into())));
        assert_eq!(r.people.len(), 4);
        // И дубля при этом не появилось.
        assert!(!r.apply(Line::Joined("B".into())));
        assert_eq!(r.people.len(), 4);
    }

    /// Собственный анонимный ник в список зрителей попадать не должен: Twitch
    /// присылает нас и в `353`, и своим `JOIN`. Живьём это выглядело как
    /// «justinfan81271» над врагом (найдено пробником 2026-08-19).
    #[test]
    fn our_own_nick_is_not_a_viewer() {
        let mut r = Roster { ignore: "justinfan81271".into(), ..Roster::default() };
        assert!(!r.apply(Line::Joined("justinfan81271".into())), "свой JOIN не считается зрителем");
        assert!(r.apply(Line::Names(vec!["justinfan81271".into(), "someone".into()])));
        assert_eq!(r.people, vec!["someone".to_string()], "остался только настоящий зритель");
    }

    /// Писавший в чат считается присутствующим, даже если JOIN мы прозевали:
    /// на людных каналах Twitch шлёт membership с задержкой или не шлёт вовсе.
    #[test]
    fn speaking_counts_as_present() {
        let mut r = Roster::default();
        assert!(r.apply(Line::Spoke("delta".into(), String::new())));
        assert_eq!(r.people, vec!["delta".to_string()]);
    }
}
