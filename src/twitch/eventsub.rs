//! WebSocket-сессия EventSub: подписка на события и их чтение.
//!
//! Порядок задан протоколом: подключиться -> получить `session_welcome` с
//! `session_id` -> только теперь создать подписку через Helix (до welcome
//! подписывать не на что) -> читать `notification`.
//!
//! Twitch сам присылает `session_reconnect`, когда хочет увести нас на другой
//! сервер, и `session_keepalive`, пока событий нет. Тишина дольше обещанного
//! keepalive означает мёртвое соединение: TCP умеет висеть молча, поэтому
//! чтение идёт с таймаутом, а не «пока не закроют».

use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use super::helix;
use super::json;
use super::{Event, Status};

pub const WS_URL: &str = "wss://eventsub.wss.twitch.tv/ws";

/// Событие, на которое подписываемся: зритель погасил награду за баллы канала.
const REDEMPTION: &str = "channel.channel_points_custom_reward_redemption.add";

/// Запас поверх обещанного keepalive: сеть на стриме дёргается, и рвать
/// рабочее соединение из-за секунды опоздания незачем.
const KEEPALIVE_GRACE: Duration = Duration::from_secs(10);

/// Потолок на открытие сокета вместе с рукопожатием.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Как часто спрашиваем полный список зрителей. Реже, чем меняется чат, и
/// сильно реже лимита Twitch (один запрос в секунду): подписи над врагами от
/// минутной задержки не страдают, а слотов у игры всё равно восемь.
const CHATTERS_EVERY: Duration = Duration::from_secs(60);

/// Как часто перепроверяем «автоматически принимать» у своих наград. Стример
/// может включить галочку посреди стрима, и до следующего подключения возврат
/// баллов молча не работал бы. Один GET в пять минут - дешевле, чем объяснять
/// зрителю, куда делись баллы.
const QUEUE_CHECK_EVERY: Duration = Duration::from_secs(300);


/// Чем закончилась сессия. Разные исходы лечатся по-разному, поэтому это не
/// `Option`: переподключение по новому адресу - не то же самое, что протухший
/// токен.
pub enum SessionEnd {
    /// Twitch уводит на другой адрес: подключиться туда, старое закрыть.
    Reconnect(String),
    /// Оборвалось или замолчало - обычный повтор с задержкой.
    Dropped,
    /// Токен не принят: обновить и попробовать заново.
    Unauthorized,
    /// Подписку отозвали (отключили награды, отозвали права) - повтор не
    /// поможет, пока стример что-то не поменяет у себя.
    Revoked,
    /// Из окна настроек попросили забыть авторизацию - рвём сессию, чтобы не
    /// ждать её естественного конца часами.
    ForgetRequested,
    /// Twitch отказался создавать подписку. Код нужен наружу: 403 тут почти
    /// всегда значит «канал не Affiliate», то есть баллов канала у него нет
    /// вовсе, и ждать событий бессмысленно.
    SubscribeFailed(u16),
}

/// Разобранное сообщение сокета.
#[derive(PartialEq, Debug)]
enum Incoming {
    Welcome { session_id: String, keepalive_secs: u64 },
    Keepalive,
    Redemption {
        viewer: String,
        reward_id: String,
        reward_title: String,
        cost: u32,
        redemption_id: String,
    },
    Reconnect { url: String },
    Revocation,
    /// Всё, что нас не касается: подтверждения, неизвестные типы.
    Other,
}

fn parse_message(text: &str) -> Incoming {
    let Some(metadata) = json::object_field(text, "metadata") else {
        return Incoming::Other;
    };
    let kind = json::str_field(metadata, "message_type").unwrap_or_default();
    let payload = json::object_field(text, "payload").unwrap_or("{}");

    match kind.as_str() {
        "session_welcome" => {
            let Some(session) = json::object_field(payload, "session") else {
                return Incoming::Other;
            };
            match json::str_field(session, "id") {
                Some(session_id) => Incoming::Welcome {
                    session_id,
                    keepalive_secs: json::i64_field(session, "keepalive_timeout_seconds").unwrap_or(10).clamp(1, 600) as u64,
                },
                None => Incoming::Other,
            }
        }
        "session_keepalive" => Incoming::Keepalive,
        "session_reconnect" => {
            let url = json::object_field(payload, "session").and_then(|s| json::str_field(s, "reconnect_url"));
            match url {
                Some(url) => Incoming::Reconnect { url },
                None => Incoming::Other,
            }
        }
        "revocation" => Incoming::Revocation,
        "notification" => {
            // Тип события берём из metadata: подписок со временем станет
            // больше одной (биты, фолловы), и разбирать их вслепую нельзя.
            if json::str_field(metadata, "subscription_type").as_deref() != Some(REDEMPTION) {
                return Incoming::Other;
            }
            let Some(event) = json::object_field(payload, "event") else {
                return Incoming::Other;
            };
            let reward = json::object_field(event, "reward").unwrap_or("{}");
            Incoming::Redemption {
                // user_name - отображаемое имя (с заглавными и не-латиницей),
                // именно его и показываем зрителям.
                viewer: json::str_field(event, "user_name")
                    .or_else(|| json::str_field(event, "user_login"))
                    .unwrap_or_default(),
                reward_id: json::str_field(reward, "id").unwrap_or_default(),
                reward_title: json::str_field(reward, "title").unwrap_or_default(),
                cost: json::u32_field(reward, "cost").unwrap_or(0),
                // Строго верхний уровень: `id` есть и у награды внутри
                // события, и перепутать их значит «вернуть» баллы не за то.
                redemption_id: json::str_field_top(event, "id").unwrap_or_default(),
            }
        }
        _ => Incoming::Other,
    }
}

/// Одна сессия целиком: подключение, подписка, чтение до обрыва.
///
/// `subscribe` - создавать ли подписку после welcome. При переезде по
/// `session_reconnect` она уже есть: Twitch переносит подписки на новую сессию
/// сам, и повторный запрос вернул бы 409, то есть ронял бы каждое плановое
/// переподключение.
pub async fn run(
    url: &str,
    client_id: &str,
    access_token: &str,
    user_id: &str,
    login: &str,
    subscribe: bool,
    status: &Mutex<Status>,
    tx: &Sender<Event>,
    commands: &super::Commands,
) -> SessionEnd {
    // Со своим таймаутом: рукопожатие с молчащим сервером иначе висит на
    // многоминутных таймаутах TCP, и всё это время мод считает себя живым.
    let connect = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url)).await;
    let Ok(Ok((mut ws, _))) = connect else {
        return SessionEnd::Dropped;
    };

    let mut subscribed = !subscribe;
    let mut last_message = Instant::now();
    // Первый опрос - сразу, а не через минуту: иначе после запуска игры
    // никнеймов не было бы целую минуту при полностью рабочем праве.
    let mut chatters_at: Option<Instant> = None;
    // Заводим «сейчас»: при подключении галочку уже проверил
    // `SweepUnfulfilled`, второй раз подряд спрашивать незачем.
    let mut queue_at: Option<Instant> = Some(Instant::now());
    // До welcome ждём недолго: молчащий сокет на этом шаге - уже неполадка.
    let mut wait = Duration::from_secs(30);

    loop {
        // Ждём короткими шагами, чтобы замечать просьбу забыть авторизацию, а
        // не висеть до следующего keepalive.
        let step = wait.min(Duration::from_secs(2));
        let next = match tokio::time::timeout(step, ws.next()).await {
            Ok(next) => next,
            Err(_) => {
                if super::FORGET_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
                    return SessionEnd::ForgetRequested;
                }
                // Интеграцию выключили в настройках - закрываем сокет здесь
                // же, а не держим его до конца сессии игры.
                if !super::enabled() {
                    return SessionEnd::Dropped;
                }
                // Пауза в сообщениях - лучший момент разгрузить очередь команд
                // от игрового потока. Пока мы ходим в Helix, сокет не читает
                // никто, поэтому засекаем это время: см. ниже.
                let busy = Instant::now();
                run_commands(commands, client_id, access_token, user_id, tx).await;
                if chatters_at.is_none_or(|at| at.elapsed() >= CHATTERS_EVERY) {
                    chatters_at = Some(Instant::now());
                    poll_chatters(client_id, access_token, user_id, login, tx).await;
                }
                if queue_at.is_none_or(|at: Instant| at.elapsed() >= QUEUE_CHECK_EVERY) {
                    queue_at = Some(Instant::now());
                    if let Some(text) = require_queue(client_id, access_token, user_id).await {
                        let _ = tx.send(Event::Notice { text });
                    }
                    // Тем же кругом сверяем метки: правку могли внести и на
                    // дашборде, и тогда «на Twitch» врало бы до перезапуска.
                    publish_marks(client_id, access_token, user_id, tx).await;
                }
                // Время, проведённое в Helix, в таймаут НЕ засчитываем.
                // Запрос там живёт до `REQUEST_TIMEOUT` (20 с), а окно
                // keepalive - ровно столько же: одна медленная пачка запросов
                // объявляла живое соединение мёртвым, при том что keepalive'ы
                // всё это время лежали в буфере сокета непрочитанными.
                // Отсюда «постоянно разрывается и соединяется» (2026-08-27).
                last_message += busy.elapsed();
                // Полный таймаут набирается из этих шагов.
                if last_message.elapsed() >= wait {
                    return SessionEnd::Dropped;
                }
                continue;
            }
        };
        last_message = Instant::now();
        let Some(Ok(msg)) = next else {
            return SessionEnd::Dropped;
        };

        let text = match msg {
            Message::Text(t) => t.to_string(),
            // Пинг обязателен к ответу: tungstenite ставит pong в очередь, но
            // отправит его только при следующей записи, а мы обычно только
            // читаем.
            Message::Ping(p) => {
                if ws.send(Message::Pong(p)).await.is_err() {
                    return SessionEnd::Dropped;
                }
                continue;
            }
            Message::Close(_) => return SessionEnd::Dropped,
            _ => continue,
        };

        match parse_message(&text) {
            Incoming::Welcome { session_id, keepalive_secs } => {
                if !subscribed {
                    let condition = helix::broadcaster_condition(user_id);
                    match helix::create_eventsub_subscription(client_id, access_token, REDEMPTION, "1", &condition, &session_id).await {
                        Ok(()) => subscribed = true,
                        Err(helix::HelixError::Unauthorized) => return SessionEnd::Unauthorized,
                        Err(helix::HelixError::Other(code, _)) => return SessionEnd::SubscribeFailed(code),
                        Err(helix::HelixError::Network) => return SessionEnd::Dropped,
                    }
                }
                wait = Duration::from_secs(keepalive_secs) + KEEPALIVE_GRACE;
                // «Подключено» ставим по факту welcome, а не по факту открытия
                // сокета: до welcome событий всё равно не будет.
                if let Ok(mut s) = status.lock() {
                    *s = Status::Connected { login: login.to_string() };
                }
            }
            Incoming::Redemption { viewer, reward_id, reward_title, cost, redemption_id } => {
                // Канал оборвался - значит игровой поток ушёл, продолжать
                // читать сокет незачем.
                let event =
                    Event::Redemption { viewer, reward_id, reward_title, cost, redemption_id };
                if tx.send(event).is_err() {
                    return SessionEnd::Dropped;
                }
            }
            Incoming::Reconnect { url } => return SessionEnd::Reconnect(url),
            Incoming::Revocation => return SessionEnd::Revoked,
            Incoming::Keepalive | Incoming::Other => {}
        }
    }
}

/// Выполняет то, что попросил игровой поток: возвраты баллов и создание
/// наград. Ошибки не роняют сессию - для возврата это просто «не получилось».
async fn run_commands(
    commands: &super::Commands,
    client_id: &str,
    access_token: &str,
    user_id: &str,
    tx: &Sender<Event>,
) {
    // Забираем всё сразу и отпускаем мьютекс: держать его на время сетевых
    // запросов значило бы подвесить игровой поток на них.
    let batch: Vec<super::Command> = match commands.lock() {
        Ok(mut q) => q.drain(..).collect(),
        Err(_) => return,
    };
    for command in batch {
        match command {
            super::Command::Refund { reward_id, redemption_id } => {
                if let Err(e) = helix::refund(client_id, access_token, user_id, &reward_id, &redemption_id).await
                {
                    let _ = tx.send(Event::RefundFailed { reason: explain_refund(&e) });
                }
            }
            super::Command::Fulfill { reward_id, redemption_id } => {
                // Помечаем ДО запроса и независимо от его исхода: смысл метки
                // - «эту покупку мод исполнил», а не «Twitch подтвердил». Не
                // прошёл PATCH по сети - покупка всё равно состоялась, и
                // возвращать за неё баллы разбору хвоста нельзя.
                super::mark_handled(&redemption_id);
                let _ = helix::fulfill(client_id, access_token, user_id, &reward_id, &redemption_id).await;
            }
            super::Command::DeleteReward { reward_id } => {
                let _ = helix::delete_reward(client_id, access_token, user_id, &reward_id).await;
            }
            // Исход не разбираем: не получилось спрятать - награду видно, а
            // покупку по ней всё равно не пропустит проверка в `redeem`.
            super::Command::EnableReward { reward_id, on } => {
                let _ = helix::set_reward_enabled(client_id, access_token, user_id, &reward_id, on).await;
            }
            super::Command::UpdateReward { local_id, reward_id, title, cost, mark } => {
                let event = match helix::update_reward(client_id, access_token, user_id, &reward_id, &title, cost).await {
                    Ok(()) => Event::RewardUpdated { local_id, mark },
                    Err(helix::HelixError::Other(404, _)) => Event::RewardFailed {
                        local_id,
                        reason: crate::i18n::t("this reward no longer exists on Twitch - create it again").to_string(),
                    },
                    Err(helix::HelixError::Other(403, message)) => Event::RewardFailed { local_id, reason: explain_403(&message) },
                    Err(e) => Event::RewardFailed { local_id, reason: format!("{} {e:?}", crate::i18n::t("could not update:")) },
                };
                let ok = matches!(event, Event::RewardUpdated { .. });
                let _ = tx.send(event);
                // Сразу спрашиваем, что там теперь на самом деле - и ПОСЛЕ
                // отправки события: иначе оптимистичная метка из
                // `RewardUpdated` перезаписала бы правду.
                if ok {
                    publish_marks(client_id, access_token, user_id, tx).await;
                }
            }
            super::Command::SweepUnfulfilled { rewards, skip } => {
                // Сначала снять «автоматически принимать», потом разбирать
                // хвост: с этой галочкой возврат не работает вовсе, и хвост
                // было бы нечем вернуть.
                if let Some(text) = require_queue(client_id, access_token, user_id).await {
                    let _ = tx.send(Event::Notice { text });
                }
                sweep(client_id, access_token, user_id, &rewards, &skip).await;
            }
            super::Command::CreateReward { local_id, title, cost, mark } => {
                let event = match helix::create_reward(client_id, access_token, user_id, &title, cost, "").await {
                    Ok(reward_id) => Event::RewardCreated { local_id, reward_id, mark },
                    Err(helix::HelixError::Other(400, _)) => Event::RewardFailed {
                        local_id,
                        // Twitch требует уникальности названий, и 400 тут почти
                        // всегда именно про это.
                        reason: crate::i18n::t("a reward with this title already exists on Twitch").to_string(),
                    },
                    Err(helix::HelixError::Other(403, message)) => Event::RewardFailed {
                        local_id,
                        reason: explain_403(&message),
                    },
                    Err(e) => Event::RewardFailed { local_id, reason: format!("{} {e:?}", crate::i18n::t("could not create:")) },
                };
                let _ = tx.send(event);
            }
        }
    }
}

/// Снимает «автоматически принимать» со своих наград на Twitch.
///
/// Стример может включить эту галочку руками, и тогда Twitch считает погашение
/// выполненным сразу: возврат баллов у такой награды не работает ничем, а
/// выглядит это как молчащий возврат (запрос 2026-08-23). Проверяем при каждом
/// подключении - один GET, а PATCH только тем, у кого она реально включена.
///
/// Возвращает текст для окна настроек, если что-то поправили: менять чужие
/// настройки молча нельзя.
/// Отпечатки того, что лежит на Twitch. Молча ничего не делаем, если запрос
/// не прошёл: метка просто останется прежней до следующего круга.
async fn publish_marks(client_id: &str, access_token: &str, user_id: &str, tx: &Sender<Event>) {
    let Ok(rows) = helix::my_rewards(client_id, access_token, user_id).await else {
        return;
    };
    let marks = rows
        .into_iter()
        .map(|(id, title, cost)| (id, super::rewards::fingerprint(&title, cost)))
        .collect();
    let _ = tx.send(Event::RewardsSynced { marks });
}

async fn require_queue(client_id: &str, access_token: &str, user_id: &str) -> Option<String> {
    let ids = helix::skipping_queue(client_id, access_token, user_id).await.ok()?;
    let mut fixed = 0;
    for id in &ids {
        if helix::require_queue(client_id, access_token, user_id, id).await.is_ok() {
            fixed += 1;
        }
    }
    (fixed > 0).then(|| {
        format!(
            "{} {fixed}: {}",
            crate::i18n::t("turned off auto-accept on rewards"),
            crate::i18n::t("otherwise refunds do not work"),
        )
    })
}

/// Возврат баллов за покупки, которые мод проспал.
///
/// Пока интеграция была выключена или связь лежала, EventSub нам ничего не
/// приносил, а Twitch пропущенное потом НЕ переигрывает: покупка навсегда
/// остаётся «не выполнена», баллы списаны, в игре не случилось ничего. Это и
/// есть «выключить-включить ломает покупку» (жалоба 2026-08-23).
///
/// Отличить проспанную от исполненной позволяет то, что исполненные мод
/// помечает `FULFILLED` (см. `Command::Fulfill`). Остаются только те, что не
/// дошли, и те, что прямо сейчас лежат в очереди игрового потока - последние
/// приходят в `skip`.
async fn sweep(client_id: &str, access_token: &str, user_id: &str, rewards: &[String], skip: &[String]) {
    for reward_id in rewards {
        let ids = match helix::unfulfilled(client_id, access_token, user_id, reward_id).await {
            Ok(ids) => ids,
            // Связи нет - остальные награды спросить всё равно не выйдет.
            Err(helix::HelixError::Network) => return,
            // А вот отказ по конкретной награде общий: Twitch отдаёт погашения
            // только тому приложению, которое эту награду создало, и одна
            // заведённая руками на дашборде не должна отменять проход по
            // остальным.
            Err(_) => continue,
        };
        for redemption_id in ids {
            if skip.contains(&redemption_id) || super::was_handled(&redemption_id) {
                continue;
            }
            let _ = helix::refund(client_id, access_token, user_id, reward_id, &redemption_id).await;
        }
    }
}

/// Полный список зрителей - тот самый, которого нет у анонимного чата.
///
/// Отказ здесь не рвёт сессию и не повторяется: 401/403 значат «право не
/// выдано» или «мы не модератор канала», а это до переавторизации не
/// изменится. Тогда никнеймы продолжают идти из чата, просто медленнее.
async fn poll_chatters(
    client_id: &str,
    access_token: &str,
    user_id: &str,
    login: &str,
    tx: &Sender<Event>,
) {
    if super::CHATTERS_DENIED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // Только свой канал: модератором чужого владелец токена не является, и
    // Twitch ответит 401 - тратить на это запрос в минуту незачем.
    let channel = super::channel();
    if channel.is_empty() || !channel.eq_ignore_ascii_case(login) {
        return;
    }
    match helix::get_chatters(client_id, access_token, user_id, user_id).await {
        Ok(list) => {
            super::CHATTERS_VIA_HELIX.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = tx.send(Event::ChattersUpdated { list, helix: true });
        }
        // Сеть отвалилась - обычное дело, попробуем через минуту.
        Err(helix::HelixError::Network) => {}
        Err(_) => {
            super::CHATTERS_DENIED.store(true, std::sync::atomic::Ordering::Relaxed);
            super::CHATTERS_VIA_HELIX.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Что на самом деле значит 403 на баллах канала.
///
/// Причин ровно две, и лечатся они противоположно. Мод раньше называл только
/// вторую («переавторизуйся»), и стример с каналом без Affiliate ходил по
/// кругу, переавторизовываясь без всякого толку (жалоба 2026-08-19).
///
/// Отличаем по тексту самого Twitch: на не-Affiliate он пишет про partner или
/// affiliate прямым текстом. Отдельного запроса для этого не нужно - сообщение
/// уже приехало в теле ответа.
/// Почему не вышло вернуть баллы.
///
/// Три внешние причины, и по одному коду их не различить - поэтому текст
/// Twitch доносится как есть, а не подменяется догадкой (то же правило, что
/// и у `explain_403`).
fn explain_refund(e: &helix::HelixError) -> String {
    use crate::i18n::t;
    match e {
        // Возвращать баллы позволено ТОЛЬКО приложению, создавшему награду.
        // Заведённую руками на дашборде мод отменить не может ничем, и
        // переавторизация тут не поможет - помогает только пересоздание
        // награды кнопкой «Создать на Twitch».
        helix::HelixError::Other(403, m) if m.is_empty() => t("Refund: Twitch refused (403). Only the app that created the reward may refund it - recreate it with Create on Twitch")
        .to_string(),
        helix::HelixError::Other(403, m) => format!("{} {m}", t("Refund, 403:")),
        // Отменить можно только покупку в статусе «не выполнено». Награда,
        // пропускающая очередь подтверждения, считается выполненной сразу.
        helix::HelixError::Other(400, _) => t("Refund: the redemption is already fulfilled. The reward must have Skip Reward Requests Queue turned OFF on the Twitch dashboard, otherwise there is nothing to cancel")
        .to_string(),
        helix::HelixError::Unauthorized => t("Refund: Twitch rejected the token - renewing and retrying")
        .to_string(),
        helix::HelixError::Network => {
            t("Refund: cannot reach Twitch").to_string()
        }
        helix::HelixError::Other(code, m) => {
            format!("{} {code} {m}", t("Refund failed:"))
        }
    }
}

fn explain_403(message: &str) -> String {
    let low = message.to_ascii_lowercase();
    if low.contains("partner") || low.contains("affiliate") {
        return crate::i18n::t("the channel has no channel points: only Affiliate and Partner do")
        .to_string();
    }
    if message.is_empty() {
        return crate::i18n::t("Twitch refused (403): the channel has no channel points (Affiliate needed), or the token has no manage-rewards scope - then use \"Forget the authorization\".")
        .to_string();
    }
    format!("{} {message}", crate::i18n::t("Twitch refused (403):"))
}

/// Разовая проверка «пустит ли Twitch подписаться»: открыть сессию, дождаться
/// welcome, создать подписку и сразу выйти.
///
/// Живёт ради `examples/twitch_check.rs`. Внутри игры это делает `run`, но там
/// результат виден только строкой статуса, а самая частая причина отказа (403 у
/// не-Affiliate) требует объяснения длиннее одной строки.
///
/// `Err(0)` - не дошли до ответа (сеть), иначе HTTP-код отказа.
pub async fn probe_subscription(client_id: &str, access_token: &str, user_id: &str) -> Result<(), u16> {
    let connect = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(WS_URL)).await;
    let Ok(Ok((mut ws, _))) = connect else {
        return Err(0);
    };
    loop {
        let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(30), ws.next()).await else {
            return Err(0);
        };
        let Message::Text(text) = msg else { continue };
        let Incoming::Welcome { session_id, .. } = parse_message(&text) else {
            continue;
        };
        let condition = helix::broadcaster_condition(user_id);
        return match helix::create_eventsub_subscription(client_id, access_token, REDEMPTION, "1", &condition, &session_id)
            .await
        {
            Ok(()) => Ok(()),
            Err(helix::HelixError::Unauthorized) => Err(401),
            Err(helix::HelixError::Other(code, _)) => Err(code),
            Err(helix::HelixError::Network) => Err(0),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 403 на баллах канала значит две противоположные вещи, и мод обязан
    /// называть ту, которая случилась: канал без Affiliate переавторизацией не
    /// чинится вовсе (жалоба 2026-08-19).
    #[test]
    fn forbidden_names_the_real_cause() {
        // Формулировка снята с живого ответа Twitch 2026-08-19, а не выдумана.
        let not_affiliate = explain_403("The broadcaster must have partner or affiliate status.");
        assert!(not_affiliate.contains("Affiliate"), "{not_affiliate}");
        // Переавторизация тут не чинит ничего, и предлагать её нельзя.
        assert!(!not_affiliate.to_lowercase().contains("authoriz"), "{not_affiliate}");

        // Пустое сообщение - называем обе причины, а не одну наугад.
        let blind = explain_403("");
        assert!(blind.contains("Affiliate") && blind.to_lowercase().contains("token"), "{blind}");

        // Чужой текст Twitch доносим как есть, а не подменяем догадкой.
        let other = explain_403("Something else entirely");
        assert!(other.contains("Something else entirely"), "{other}");
    }

    #[test]
    fn reads_welcome() {
        let m = r#"{"metadata":{"message_id":"1","message_type":"session_welcome","message_timestamp":"t"},
                    "payload":{"session":{"id":"sess","status":"connected",
                    "connected_at":"t","keepalive_timeout_seconds":10,"reconnect_url":null}}}"#;
        assert_eq!(
            parse_message(m),
            Incoming::Welcome { session_id: "sess".into(), keepalive_secs: 10 }
        );
    }

    #[test]
    fn reads_a_redemption() {
        let m = r#"{"metadata":{"message_type":"notification",
                    "subscription_type":"channel.channel_points_custom_reward_redemption.add"},
                    "payload":{"subscription":{"id":"s"},"event":{
                    "id":"redemption-1","broadcaster_user_id":"1","user_id":"77",
                    "user_login":"nightrider","user_name":"NightRider","user_input":"go",
                    "status":"unfulfilled","reward":{"id":"rw-9","title":"Прыжок","cost":150,"prompt":""},
                    "redeemed_at":"t"}}}"#;
        assert_eq!(
            parse_message(m),
            Incoming::Redemption {
                viewer: "NightRider".into(),
                reward_id: "rw-9".into(),
                reward_title: "Прыжок".into(),
                cost: 150,
                redemption_id: "redemption-1".into(),
            }
        );
    }

    /// Событие чужого типа не должно притворяться покупкой награды.
    #[test]
    fn ignores_other_subscription_types() {
        let m = r#"{"metadata":{"message_type":"notification","subscription_type":"channel.follow"},
                    "payload":{"event":{"user_name":"someone"}}}"#;
        assert_eq!(parse_message(m), Incoming::Other);
    }

    #[test]
    fn reads_keepalive_reconnect_and_revocation() {
        assert_eq!(
            parse_message(r#"{"metadata":{"message_type":"session_keepalive"},"payload":{}}"#),
            Incoming::Keepalive
        );
        assert_eq!(
            parse_message(
                r#"{"metadata":{"message_type":"session_reconnect"},
                    "payload":{"session":{"id":"s","reconnect_url":"wss://new.example/ws"}}}"#
            ),
            Incoming::Reconnect { url: "wss://new.example/ws".into() }
        );
        assert_eq!(
            parse_message(r#"{"metadata":{"message_type":"revocation"},"payload":{"subscription":{"status":"user_removed"}}}"#),
            Incoming::Revocation
        );
    }

    /// Мусор из сокета не должен ронять поток: в релизе panic = "abort".
    #[test]
    fn garbage_is_ignored() {
        for m in ["", "{}", "не json", r#"{"metadata":{}}"#, r#"{"metadata":{"message_type":"session_welcome"}}"#] {
            assert_eq!(parse_message(m), Incoming::Other, "{m}");
        }
    }

    /// Событие без названия награды всё равно должно доехать: сопоставить его
    /// можно по id, а пустое имя - не повод терять покупку.
    #[test]
    fn redemption_survives_missing_optional_fields() {
        let m = r#"{"metadata":{"message_type":"notification",
                    "subscription_type":"channel.channel_points_custom_reward_redemption.add"},
                    "payload":{"event":{"reward":{"id":"rw"}}}}"#;
        assert_eq!(
            parse_message(m),
            Incoming::Redemption {
                viewer: String::new(),
                reward_id: "rw".into(),
                reward_title: String::new(),
                cost: 0,
                redemption_id: String::new(),
            }
        );
    }
}
