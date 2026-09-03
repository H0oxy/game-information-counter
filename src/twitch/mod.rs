//! Интеграция с Twitch: зритель тратит баллы канала - в игре что-то происходит.
//!
//! Устройство то же, что у веб-выхода: своя работа в своём потоке, наружу
//! только плоские данные. Игровой поток (`render`) никогда не ждёт сети - он
//! вычерпывает канал неблокирующим `try_recv` и читает статус под коротким
//! `lock`.
//!
//! Разделение внутри модуля: чистая логика (разбор JSON, сопоставление наград,
//! очередь действий) - свободные функции над плоскими данными, без `Arc`,
//! `Mutex` и tokio в сигнатурах; сеть - тонкая обвязка поверх них. Так же
//! устроены `config::rewrite_values` и `stats::decode_attempt`, и ровно это
//! позволяет покрыть логику `cargo test` без сети и без игры.

pub mod actions;
pub mod auth;
pub mod chat;
pub mod eventsub;
pub mod helix;
pub mod http;
pub mod json;
pub mod rewards;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use auth::{PollOutcome, RefreshError, TwitchToken};
use crate::i18n::t;

/// Права, которые запрашиваем.
///
/// `moderator:read:chatters` - ради полного списка зрителей (`helix::get_chatters`):
/// анонимный чат показывает только тех, кто пишет, и на людном канале список
/// набирается единицами имён в минуту. Право зашивается в токен в момент
/// выдачи, поэтому уже выданный его не получит - нужна «Забыть авторизацию».
pub const SCOPE_REDEMPTIONS: &str =
    "channel:read:redemptions channel:manage:redemptions moderator:read:chatters";

/// Канал, чей чат слушаем. Ставит игровой поток при смене настройки, читает
/// сетевой: полный список зрителей Twitch отдаёт только там, где владелец
/// токена модератор, то есть на своём канале.
static CHANNEL: Mutex<String> = Mutex::new(String::new());

pub fn set_channel(name: &str) {
    if let Ok(mut c) = CHANNEL.lock() {
        *c = name.to_string();
    }
}

fn channel() -> String {
    CHANNEL.lock().map(|c| c.clone()).unwrap_or_default()
}

/// Client ID приложения. Живёт глобально, а не параметром потока: его меняют
/// прямо в окне настроек кнопкой «Подключить», и «перезапусти игру» в ответ на
/// исправленную опечатку - плохой ответ.
static CLIENT_ID: Mutex<String> = Mutex::new(String::new());

/// Ставит игровой поток каждый кадр. Смена приложения обесценивает выданный им
/// токен, поэтому идёт вместе со сбросом авторизации - но не на первой
/// установке, иначе сохранённый токен стирался бы при каждом запуске игры.
pub fn set_client_id(id: &str) {
    let mut cur = CLIENT_ID.lock().unwrap_or_else(|e| e.into_inner());
    if *cur == id {
        return;
    }
    if !cur.is_empty() {
        request_forget();
    }
    *cur = id.to_string();
}

fn client_id() -> String {
    CLIENT_ID.lock().map(|c| c.clone()).unwrap_or_default()
}

/// Список зрителей приходит из Helix, а не из анонимного чата. Читается окном
/// настроек: разница между «40 человек за полчаса» и «все сразу» слишком
/// велика, чтобы о ней молчать.
pub static CHATTERS_VIA_HELIX: AtomicBool = AtomicBool::new(false);

/// Helix отказал в списке зрителей: у токена нет `moderator:read:chatters`
/// (выдан до того, как право добавили) или мы не модератор канала. Спрашивать
/// дальше бессмысленно - до переавторизации ответ не изменится.
pub static CHATTERS_DENIED: AtomicBool = AtomicBool::new(false);

/// Включена ли интеграция прямо сейчас. Ставит игровой поток каждый кадр по
/// галочке в настройках.
///
/// Раньше галочка «Включить интеграцию» после запуска потока не значила
/// ничего: соединение продолжало жить, покупки продолжали исполняться, и
/// выключить это можно было только перезапуском игры. Поток не убиваем, а
/// усыпляем - следить за тем, умер ли прошлый, прежде чем поднимать новый,
/// дороже, чем спать секунду в цикле.
static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Что прилетело из Twitch. Отдаётся в `render` через `std::sync::mpsc`:
/// отправителей несколько (авторизация, поток событий, опрос чата),
/// получатель один - ровно случай mpsc, `crossbeam` тут не нужен.
pub enum Event {
    /// Зритель погасил награду за баллы канала.
    Redemption {
        viewer: String,
        reward_id: String,
        reward_title: String,
        cost: u32,
        /// Id самого погашения - только по нему можно вернуть баллы.
        redemption_id: String,
    },
    /// Обновился список зрителей в чате - из него берутся никнеймы для врагов.
    /// `helix` - список полный (все, включая молчащих), а не только писавшие.
    ChattersUpdated { list: Vec<String>, helix: bool },
    /// Награда создана на Twitch: её id надо запомнить в файле наград, чтобы
    /// сопоставлять покупки точно, а не по названию.
    ///
    /// `mark` - отпечаток того, что мы отправили; он и запоминается как «на
    /// Twitch лежит это». Возвращается сетевым потоком как есть, а не
    /// считается заново: пока запрос летел, цену могли поправить ещё раз.
    RewardCreated { local_id: u32, reward_id: String, mark: u64 },
    /// Создать награду не удалось - сказать об этом в окне настроек.
    RewardFailed { local_id: u32, reason: String },
    /// Награда на Twitch приведена в соответствие с настройками мода.
    RewardUpdated { local_id: u32, mark: u64 },
    /// Что у наград лежит на Twitch на самом деле: `reward_id` -> отпечаток.
    /// Метку синхронизации ставим по нему, а не по тому, что мы отправили -
    /// PATCH мог и не примениться.
    RewardsSynced { marks: Vec<(String, u64)> },
    /// Вернуть баллы не вышло - и это надо показать, а не проглотить.
    ///
    /// Возврат молча не работал, а причин у этого несколько и все внешние:
    /// награду создало другое приложение, покупка уже выполнена, у канала нет
    /// баллов вовсе. Молчащий возврат неотличим от работающего до тех пор,
    /// пока зритель не пожалуется (живьём 2026-08-23).
    RefundFailed { reason: String },
    /// Мод что-то поправил на Twitch сам - сказать об этом, а не менять чужие
    /// настройки молча.
    Notice { text: String },
}

/// Что игровой поток просит сделать сетевой. Обратное направление канала:
/// возврат баллов и создание наград - это запросы к Twitch, а решает про них
/// игровая логика.
///
/// Очередь под `Mutex`, а не второй канал: сетевой поток и так просыпается
/// каждые пару секунд, и забирать команды оттуда дешевле, чем тащить
/// асинхронный receiver через все слои.
pub type Commands = Arc<Mutex<std::collections::VecDeque<Command>>>;

pub enum Command {
    /// Вернуть баллы за неисполненную покупку.
    Refund { reward_id: String, redemption_id: String },
    /// Пометить покупку выполненной. Без этого она навсегда остаётся у Twitch
    /// в статусе «не выполнено», и разбор хвоста (`SweepUnfulfilled`) не смог
    /// бы отличить исполненную покупку от проспанной.
    Fulfill { reward_id: String, redemption_id: String },
    /// Создать награду на дашборде Twitch.
    CreateReward { local_id: u32, title: String, cost: u32, mark: u64 },
    /// Привести уже созданную награду к тому, что настроено в моде: цена,
    /// название, включена ли она.
    UpdateReward { local_id: u32, reward_id: String, title: String, cost: u32, mark: u64 },
    /// Убрать награду с дашборда - вслед за удалением её из списка мода.
    DeleteReward { reward_id: String },
    /// Показать награду зрителям или спрятать. Отдельно от `UpdateReward`:
    /// тот `is_enabled` не шлёт намеренно (см. `helix::update_reward`), и
    /// доступностью распоряжается стример - кроме двух случаев, когда за него
    /// это делает мод: перезарядка награды и кнопка «Выключить все».
    EnableReward { reward_id: String, on: bool },
    /// Разобрать хвост: вернуть баллы за покупки, пришедшие пока мод не
    /// слушал. `skip` - те, что прямо сейчас в очереди игрового потока: они
    /// ещё исполнятся, возвращать их нельзя.
    SweepUnfulfilled { rewards: Vec<String>, skip: Vec<String> },
}

/// Состояние подключения для окна настроек. Пишется сетевым потоком, читается
/// раз в кадр игровым - как `Shared` у веб-выхода.
#[derive(Clone, Default)]
pub enum Status {
    /// Интеграция выключена в настройках.
    #[default]
    Disabled,
    /// Не введён Client ID - подключаться нечем.
    NotConfigured,
    /// Client ID есть, сохранённого токена нет: ждём кнопку «Подключить».
    /// Код на twitch.tv/activate вводит человек, и начинать это самотёком,
    /// стоило появиться Client ID, незачем.
    NeedsAuth,
    /// Ждём, пока стример введёт код на twitch.tv/activate.
    AwaitingDeviceCode { user_code: String, verification_uri: String },
    Connecting,
    Connected { login: String },
    /// Последняя ошибка и через сколько секунд следующая попытка. Соединение
    /// не сдаётся насовсем: сеть на стриме отваливается и возвращается.
    Error { message: String, retry_in_secs: u64 },
}

/// Покупка, показанная на экране. Живёт недолго: карточка гаснет через
/// `twitch_notify_secs`, старые вычищаются.
#[derive(Clone)]
pub struct PurchaseEvent {
    pub viewer: String,
    /// Что показывать. Подпись из файла наград, если она задана, иначе
    /// название награды с Twitch.
    pub label: String,
    /// Ноль - карточка не о покупке, а о состоянии мода (связь с Twitch
    /// пропала, вернулась). Цена справа тогда не рисуется: платить было
    /// некому. Настоящая награда нулевой не бывает - Twitch требует минимум
    /// единицу.
    pub cost: u32,
    pub at: Instant,
    /// До какого момента карточка показывает обратный отсчёт.
    ///
    /// У отложенного спавна это когда враг появится: зрителям видно, сколько
    /// осталось, а стримеру хватает времени добежать до удобного места. У
    /// временного эффекта - когда он кончится (запрос 2026-08-24): карточка
    /// висит всё его время и всё это время показывает, сколько ещё терпеть.
    ///
    /// Поле одно на оба смысла намеренно: рисуется отсчёт одинаково, а что
    /// именно кончится, читается по названию награды рядом.
    pub countdown_to: Option<Instant>,
    /// Сколько карточке висеть, если её покупка задаёт своё время.
    /// `None` - общее `twitch_notify_secs`.
    ///
    /// Нужно эффектам: пока «Замедлить игру» идёт сто секунд, зритель должен
    /// видеть, чьих это рук дело, а не пять секунд из ста (запрос
    /// 2026-08-24). Живёт тут, а не в `Action`, потому что карточку заводит
    /// `redeem` ещё до того, как выяснится, исполнится ли покупка вообще.
    pub life: Option<f32>,
    /// Погашение, за которое эта карточка. Нужно ровно для одного: снять её,
    /// если баллы за покупку вернули (`StreamHud::refund`). Показывать
    /// «зритель купил», когда покупка не состоялась и деньги отданы обратно, -
    /// враньё на экране стрима. Пусто у тестовой покупки из окна настроек.
    pub redemption_id: String,
}

impl PurchaseEvent {
    /// Сколько эта карточка живёт: своё время важнее общего.
    ///
    /// **Одна функция на все три места, где это считается** - чистка списка в
    /// `StreamHud`, отрисовка в игре и JSON для OBS. Разъехавшись, они дали
    /// ровно то, на что и пожаловались (2026-08-24): отрисовка честно держала
    /// карточку эффекта весь его срок и показывала отсчёт, а чистка выбрасывала
    /// её через общие несколько секунд. Тот же класс, что «одна таблица клавиш
    /// вместо двух списков».
    pub fn life_secs(&self, shared: f32) -> f32 {
        self.life.unwrap_or(shared)
    }

    /// Не догорела ли карточка.
    pub fn alive(&self, shared: f32) -> bool {
        self.at.elapsed().as_secs_f32() < self.life_secs(shared)
    }
}

#[cfg(test)]
mod purchase_tests {
    use super::*;

    /// Срок карточки: своё время перебивает общее, а без своего берётся общее.
    /// Правило одно на чистку списка, отрисовку и JSON - см. `life_secs`.
    #[test]
    fn a_cards_own_life_wins_over_the_shared_one() {
        let card = |life| PurchaseEvent {
            viewer: String::new(),
            label: String::new(),
            cost: 0,
            at: Instant::now(),
            countdown_to: None,
            life,
            redemption_id: String::new(),
        };
        assert_eq!(card(None).life_secs(6.0), 6.0);
        assert_eq!(card(Some(180.0)).life_secs(6.0), 180.0);
        // Свежая карточка жива при любом из двух сроков.
        assert!(card(None).alive(6.0));
        assert!(card(Some(180.0)).alive(6.0));
        // Нулевой срок - карточки нет вовсе, и своё тут тоже главнее.
        assert!(!card(Some(0.0)).alive(6.0));
    }
}

/// Пауза перед следующей попыткой. Растёт до получаса-минуты, но не дальше:
/// сеть на стриме отваливается и возвращается, и мод обязан сам подняться,
/// не требуя перезапуска игры.
fn backoff(failures: u32) -> Duration {
    const STEPS: [u64; 5] = [1, 2, 5, 15, 30];
    Duration::from_secs(STEPS[(failures as usize).min(STEPS.len() - 1)])
}

/// Поднимает сетевой поток. Зовётся из `render` один раз за сессию - лениво,
/// как `web::spawn`, а не из `DllMain`: там уже тесно от гонки на старте, и
/// провал `Hudhook::apply` уводит мод в `eject`.
/// Просьба забыть токен, выставляемая из окна настроек.
///
/// Флаг, а не удаление файла: файл сетевой поток уже прочитал, и токен живёт у
/// него в памяти - стереть файл значило бы ничего не поменять до перезапуска
/// игры (ровно это и было жалобой «забыть авторизацию не работает»).
pub static FORGET_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Покупки, которые мод в этой сессии уже исполнил.
///
/// Нужны разбору хвоста: он возвращает баллы за всё, что у Twitch числится
/// «не выполнено», а туда попадает и покупка, чей `Fulfill` не прошёл по сети.
/// Без этого списка зрителю вернули бы баллы за то, что в игре случилось.
///
/// ponytail: последние `HANDLED_CAP` штук, дальше самые старые вытесняются.
/// Разбор хвоста смотрит на минуты, а не на часы, и глубже помнить незачем.
static HANDLED: Mutex<Vec<String>> = Mutex::new(Vec::new());
const HANDLED_CAP: usize = 256;

pub fn mark_handled(redemption_id: &str) {
    if redemption_id.is_empty() {
        return;
    }
    let Ok(mut seen) = HANDLED.lock() else { return };
    if seen.len() >= HANDLED_CAP {
        seen.remove(0);
    }
    seen.push(redemption_id.to_string());
}

pub fn was_handled(redemption_id: &str) -> bool {
    HANDLED.lock().map(|seen| seen.iter().any(|s| s == redemption_id)).unwrap_or(false)
}

pub fn request_forget() {
    FORGET_REQUESTED.store(true, Ordering::Relaxed);
}

/// Просьба начать новую авторизацию, из окна настроек. Готовому токену она не
/// нужна: «уже подключались» значит подключаемся молча, при живом токене и
/// включённой галочке.
static CONNECT_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request_connect() {
    CONNECT_REQUESTED.store(true, Ordering::Relaxed);
}

fn take_connect_request() -> bool {
    CONNECT_REQUESTED.swap(false, Ordering::Relaxed)
}

fn take_forget_request() -> bool {
    FORGET_REQUESTED.swap(false, Ordering::Relaxed)
}

pub fn spawn(
    hmodule: usize,
    scopes: String,
    status: Arc<Mutex<Status>>,
    tx: Sender<Event>,
    commands: Commands,
) {
    std::thread::spawn(move || {
        // Один поток, одно соединение и редкие запросы - пул потоков тут не
        // нужен, поэтому `current_thread`, а не `rt-multi-thread`.
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            if let Ok(mut s) = status.lock() {
                *s = Status::Error { message: t("could not start the network thread").into(), retry_in_secs: 0 };
            }
            return;
        };
        rt.block_on(run(hmodule, scopes, status, tx, commands));
    });
}

fn set_status(status: &Mutex<Status>, value: Status) {
    if let Ok(mut s) = status.lock() {
        *s = value;
    }
}

/// Главный цикл: держать соединение живым, что бы ни случилось.
///
/// Ни один путь отсюда не выходит наружу и ни один не крутится без паузы -
/// busy-loop на игровой машине хуже, чем отсутствие интеграции.
async fn run(
    hmodule: usize,
    scopes: String,
    status: Arc<Mutex<Status>>,
    tx: Sender<Event>,
    commands: Commands,
) {
    let mut failures = 0u32;
    let mut token = auth::load_token(hmodule);

    loop {
        let wait = backoff(failures);

        // Интеграцию выключили галочкой - спим, ничего не делаем и не держим
        // соединение. Просыпаемся сами, когда включат обратно.
        while !enabled() {
            set_status(&status, Status::Disabled);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        if take_forget_request() {
            auth::forget_token(hmodule);
            token = None;
        }

        // Client ID читается каждый круг, а не один раз при запуске потока:
        // его правят в окне настроек, и подключаться надо тем, что там сейчас.
        let client_id = client_id();
        if client_id.is_empty() {
            set_status(&status, Status::NotConfigured);
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        // Новый круг - новый токен: право на список зрителей могло появиться
        // ровно сейчас, ради него авторизацию и сбрасывают.
        CHATTERS_DENIED.store(false, Ordering::Relaxed);

        set_status(&status, Status::Connecting);
        let active = match obtain_token(hmodule, &client_id, &scopes, &mut token, &status).await {
            Ok(t) => t,
            Err(why) => {
                // Сброс авторизации и выключенная галочка - не сетевая
                // неудача: круг начинается заново сразу, без паузы backoff.
                if FORGET_REQUESTED.load(Ordering::Relaxed) || !enabled() {
                    continue;
                }
                fail(&status, &why, wait, &mut failures).await;
                continue;
            }
        };

        let (user_id, login) = match helix::get_own_user(&client_id, &active.access_token).await {
            Ok(v) => v,
            Err(helix::HelixError::Unauthorized) => {
                // Токен мёртв раньше срока. НЕ выбрасываем его: refresh-токен
                // при этом обычно жив, и молча обновиться куда лучше, чем
                // гнать стримера вводить код заново. Если и refresh не примут,
                // `obtain_token` сам сотрёт файл и запустит авторизацию.
                expire(&mut token);
                fail(&status, t("Twitch rejected the token"), wait, &mut failures).await;
                continue;
            }
            Err(helix::HelixError::Other(code, _)) => {
                fail(&status, &format!("{} {code}", t("Twitch answered the profile request:")), wait, &mut failures).await;
                continue;
            }
            Err(_) => {
                fail(&status, t("cannot reach api.twitch.tv"), wait, &mut failures).await;
                continue;
            }
        };

        // Сессия живёт, пока Twitch не уведёт нас на другой адрес или не
        // оборвёт. Переезд по `session_reconnect` - не ошибка и паузы не
        // требует, поэтому он внутри своего цикла.
        let mut url = eventsub::WS_URL.to_string();
        let mut connected = false;
        // Подписываемся только на первой сессии: на новый адрес Twitch
        // переносит подписки сам.
        let mut subscribe = true;
        // Причина, по которой сессия закончилась. Осмысленная переживает
        // общий текст «соединение потеряно»: именно она объясняет стримеру,
        // что чинить.
        let reason;
        loop {
            let end = eventsub::run(
                &url,
                &client_id,
                &active.access_token,
                &user_id,
                &login,
                subscribe,
                &status,
                &tx,
                &commands,
            )
            .await;
            match end {
                eventsub::SessionEnd::Reconnect(next) => {
                    connected = true;
                    url = next;
                    subscribe = false;
                }
                eventsub::SessionEnd::Unauthorized => {
                    expire(&mut token);
                    reason = t("Twitch rejected the token, renewing").to_string();
                    break;
                }
                eventsub::SessionEnd::Revoked => {
                    reason = t("the subscription was revoked, check the mod's Twitch scopes").to_string();
                    break;
                }
                eventsub::SessionEnd::SubscribeFailed(code) => {
                    // 403 здесь - почти всегда «канал не Affiliate»: баллов
                    // канала у него нет вовсе, и никакая настройка мода этого
                    // не изменит. Пишем прямо, иначе это выглядит как поломка.
                    reason = if code == 403 {
                        "Twitch не даёт подписаться на баллы канала (403). Скорее всего \
                         канал не Affiliate - тогда баллов канала у него нет вовсе. \
                         Показ проверяется кнопкой «тестовая покупка»"
                            .to_string()
                    } else {
                        format!("{} ({code})", t("could not subscribe to events"))
                    };
                    break;
                }
                eventsub::SessionEnd::Dropped => {
                    reason = t("connection lost").to_string();
                    break;
                }
                eventsub::SessionEnd::ForgetRequested => {
                    auth::forget_token(hmodule);
                    token = None;
                    reason = t("authorization was reset").to_string();
                    break;
                }
            }
        }

        // Соединение, продержавшееся до реальной работы, обнуляет счётчик:
        // иначе редкие обрывы за долгий стрим накопились бы в получасовую
        // паузу на ровном месте.
        if connected {
            failures = 0;
        }
        // Сессию оборвали галочкой, а не сетью: это не неудача, паузы не
        // требует и счётчик не двигает.
        if !enabled() {
            continue;
        }
        fail(&status, &reason, backoff(failures), &mut failures).await;
    }
}

/// Помечает кэшированный токен просроченным, не удаляя его: следующий
/// `obtain_token` пойдёт обновлять по refresh, а не заново авторизовываться.
fn expire(token: &mut Option<TwitchToken>) {
    if let Some(t) = token.as_mut() {
        t.expires_at = 0;
    }
}

async fn fail(status: &Mutex<Status>, message: &str, wait: Duration, failures: &mut u32) {
    set_status(status, Status::Error { message: message.to_string(), retry_in_secs: wait.as_secs() });
    *failures = failures.saturating_add(1);
    tokio::time::sleep(wait).await;
}

/// Действующий токен: из файла, обновлением или новой авторизацией.
async fn obtain_token(
    hmodule: usize,
    client_id: &str,
    scopes: &str,
    cached: &mut Option<TwitchToken>,
    status: &Mutex<Status>,
) -> Result<TwitchToken, String> {
    if let Some(t) = cached.clone() {
        if !t.needs_renewal(crate::stats::unix_now()) {
            return Ok(t);
        }
        match auth::refresh(client_id, &t.refresh_token).await {
            Ok(fresh) => {
                auth::save_token(hmodule, &fresh);
                *cached = Some(fresh.clone());
                return Ok(fresh);
            }
            // Обновить не вышло по сети - старый токен может быть ещё жив,
            // но лезть с ним смысла нет: следующий круг попробует снова.
            Err(RefreshError::Network) => return Err(crate::i18n::t("no connection to Twitch while renewing the token").into()),
            Err(RefreshError::Rejected) => {
                auth::forget_token(hmodule);
                *cached = None;
            }
        }
    }

    // Токена нет - дальше только с ведома стримера. Ждём кнопку: сама по себе
    // новая авторизация начинаться не должна (запрос 2026-08-22).
    loop {
        // Просьбу сбросить проверяем ПЕРВОЙ и просьбу подключиться не съедаем:
        // смена Client ID кнопкой приходит вместе с обеими, и круг обязан
        // начаться заново с новым id, унеся нажатие с собой.
        if !enabled() || FORGET_REQUESTED.load(Ordering::Relaxed) {
            return Err(t("connecting was cancelled").into());
        }
        if take_connect_request() {
            break;
        }
        set_status(status, Status::NeedsAuth);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let fresh = device_flow(client_id, scopes, status).await?;
    auth::save_token(hmodule, &fresh);
    *cached = Some(fresh.clone());
    Ok(fresh)
}

/// Авторизация кодом устройства: показываем стримеру код и ждём, пока он
/// введёт его на сайте.
async fn device_flow(client_id: &str, scopes: &str, status: &Mutex<Status>) -> Result<TwitchToken, String> {
    let device = auth::request_device_code(client_id, scopes).await?;
    set_status(status, Status::AwaitingDeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
    });

    let deadline = Instant::now() + device.expires_in;
    let mut interval = device.interval;
    while Instant::now() < deadline {
        // Спим шагами, а не одним сном на весь интервал опроса: «Забыть
        // авторизацию» и снятая галочка посреди ожидания кода обязаны
        // срабатывать сразу, а не через пять секунд (жалоба 2026-08-22).
        let until = Instant::now() + interval;
        while Instant::now() < until {
            if FORGET_REQUESTED.load(Ordering::Relaxed) || !enabled() {
                return Err(t("waiting for the code was cancelled").into());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        match auth::poll_token(client_id, &device.device_code).await {
            PollOutcome::Token(t) => return Ok(t),
            PollOutcome::Pending => {}
            // Twitch просит сбавить темп - именно просит, а не ругается:
            // продолжаем, но реже.
            PollOutcome::SlowDown => interval += Duration::from_secs(1),
            PollOutcome::Denied => return Err(t("access was denied on the Twitch site").into()),
            PollOutcome::Expired => return Err(t("the code expired, taking a new one").into()),
            // Сетевая неудача не должна съедать код: он живёт полчаса, и
            // разумнее подождать, чем выбрасывать и просить новый.
            PollOutcome::Failed => interval = interval.max(Duration::from_secs(5)),
        }
    }
    Err(t("the code expired, taking a new one").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Отклонённый токен обязан вести к обновлению, а не к новой авторизации:
    /// refresh-токен при этом обычно жив, и гнать стримера вводить код заново
    /// незачем. Раньше на этом месте стирался файл целиком.
    #[test]
    fn rejected_token_is_renewed_not_dropped() {
        let mut token = Some(TwitchToken {
            access_token: "dead".into(),
            refresh_token: "still-good".into(),
            // По времени токен ещё «живой» - без пометки обновление не
            // запустилось бы, и мы крутились бы с мёртвым по кругу.
            expires_at: u64::MAX,
        });
        expire(&mut token);
        let t = token.expect("токен остаётся - он нужен ради refresh_token");
        assert_eq!(t.refresh_token, "still-good");
        assert!(t.needs_renewal(0), "помечен просроченным, следующий круг пойдёт обновлять");
    }

    /// Смена приложения обесценивает выданный им токен, поэтому идёт вместе
    /// со сбросом авторизации. Первая установка сменой НЕ является: иначе
    /// сохранённый токен стирался бы при каждом запуске игры.
    #[test]
    fn changing_the_client_id_forgets_the_token_but_the_first_one_does_not() {
        FORGET_REQUESTED.store(false, Ordering::Relaxed);
        set_client_id("aaa");
        assert!(!take_forget_request(), "первая установка - не смена приложения");
        set_client_id("aaa");
        assert!(!take_forget_request(), "то же значение - не смена");
        set_client_id("bbb");
        assert!(take_forget_request(), "новому приложению старый токен не годится");
        set_client_id("");
    }

    /// Пауза растёт, но упирается в потолок: мод обязан сам подняться, когда
    /// сеть вернётся, а не уйти в многочасовое ожидание.
    #[test]
    fn backoff_grows_and_stops_growing() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(4), Duration::from_secs(30));
        assert_eq!(backoff(4), backoff(999), "потолок, а не бесконечный рост");
        assert_eq!(backoff(u32::MAX), Duration::from_secs(30));
    }
}
