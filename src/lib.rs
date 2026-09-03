//! Game Information Counter - HUD статистики Elden Ring для стримов.
//!
//! Грузится любым способом, который делает `LoadLibrary` в `eldenring.exe`:
//! Elden Mod Loader, ModEngine2 (`external_dlls`), ME3 (`[[natives]]`).
//! Регистрировать только в ОДНОМ загрузчике: второй `DllMain` поднимет второй
//! Hudhook поверх первого.
//!
//! Игру мы только читаем: ни одного детура ради данных, ни AOB-скана. Патчим
//! ровно в одном месте и не ради чтения - `input.rs` глушит игровой ввод, пока
//! открыто окно настроек. Всё остальное, на что уходят недели в модах с
//! детурами - гонка на прологе, дедлок `SuspendThread`, распатчивание на
//! выгрузке, - сюда не переехало.

mod bosses;
mod config;
mod effects;
mod enemies;
mod i18n;
mod input;
mod msg;
mod overlay;
mod secret;
mod settings;
mod spawn;
mod stats;
mod web;

// Публичный только ради `examples/twitch_check.rs`: проверять подключение
// изнутри игры нечем - там нет ни консоли, ни лога, а вслепую эти три шага
// (код устройства, токен, подписка) не различить.
pub mod twitch;

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eldenring::util::system::wait_for_system_init;
use fromsoftware_shared::program::Program;
use hudhook::hooks::dx12::ImguiDx12Hooks;
use hudhook::imgui::{Context, Ui};
use hudhook::{eject, windows, Hudhook, ImguiRenderLoop, RenderContext};

use config::Config;
use stats::{Collector, Snapshot};
use web::Shared;

struct StreamHud {
    /// Наш собственный `HMODULE` - нужен, чтобы найти `.ini` рядом с DLL.
    /// `GetModuleFileNameW(None, ..)` вернул бы путь к exe самой игры.
    hmodule: usize,
    collector: Collector,
    /// То же, что видит веб-страница. Оверлей читает снимок отсюда же, чтобы
    /// две картинки не разъезжались на кадр.
    shared: Arc<Mutex<Shared>>,
    config: Config,
    font_signature: String,
    font_reload_pending: bool,
    web_started: bool,
    /// Сколько кадров отрисовано - показывает диагностическое окно (`debug`).
    frames: u64,
    /// Текущая прозрачность панели, 0..1. Панель гаснет и проявляется, а не
    /// щёлкает: при заходе в инвентарь рывок читается как баг отрисовки.
    hud_fade: f32,
    /// Сколько секунд подряд память читается. Панель не показывается, пока это
    /// число не перевалит `WARMUP_SECS`: при загрузке данные становятся
    /// читаемыми раньше, чем игра выставит флаги меню и катсцены.
    valid_for: f32,
    /// Сколько секунд подряд состояние выглядит показываемым. Гасим сразу, а
    /// показываем с задержкой: на загрузках флаги дёргаются на кадр-другой.
    showable_for: f32,
    settings_open: bool,
    /// Прозрачность окна настроек, 0..1. Окно проявляется и гаснет тем же
    /// жестом, что и панель, поэтому пока оно гаснет, его надо продолжать
    /// рисовать - уже без ввода (см. `settings::draw`).
    settings_fade: f32,
    /// Состояние перетаскивания ручки положения панели - между кадрами, у
    /// панели одна ручка на весь мод.
    panel_drag: Option<overlay::PadDrag>,

    /// Сетевой поток Twitch поднят. Как `web_started` - один раз за сессию.
    twitch_started: bool,
    /// Состояние подключения: пишет сетевой поток, читает окно настроек.
    twitch_status: Arc<Mutex<twitch::Status>>,
    /// В `Mutex` не ради синхронизации - читает его только render-поток.
    /// hudhook требует от render loop `Sync`, а `Receiver` его не имеет: это
    /// та же порода ограничения, что и запрет хранить `FontId` (см. overlay).
    twitch_rx: Mutex<Receiver<twitch::Event>>,
    /// Оригинал отправителя: клон уходит в сетевой поток. Хранится, потому что
    /// поток поднимается позже создания канала.
    twitch_tx: Sender<twitch::Event>,
    /// Показанные покупки, новые в конце. Длина ограничена: за долгий стрим
    /// их накопятся тысячи, а на экране живут секунды.
    purchases: Vec<twitch::PurchaseEvent>,
    /// Журнал состоявшихся покупок для окна настроек, новые в конце. Отдельно
    /// от `purchases`: тот каждый кадр чистится по `twitch_notify_secs`, и
    /// пока история читалась из него, она жила ровно столько же, сколько
    /// карточка на экране (жалоба 2026-09-03).
    purchase_log: std::collections::VecDeque<String>,
    /// Список изменился с прошлой публикации в `Shared`. Возраст карточки
    /// веб-страница считает сама в момент запроса, поэтому копировать список
    /// каждый кадр незачем - только когда что-то пришло или догорело.
    purchases_dirty: bool,
    /// Сколько раз нажали «тестовая покупка» - только чтобы карточки
    /// отличались номером.
    test_purchases: u32,
    /// Что зрители могут купить. Отдельный файл, не `.ini`: список
    /// произвольной длины.
    rewards: Vec<twitch::rewards::RewardEntry>,
    /// Зажатые прямо сейчас клавиши.
    actions: twitch::actions::ActionState,
    /// Заспавненные за баллы враги: кто ждёт подтверждения от движка, кто уже
    /// в мире и досчитывает TTL.
    spawn: spawn::SpawnState,
    effects: effects::EffectState,
    /// Двигают ручку положения карточки покупки - показать образец на экране.
    preview_toast: bool,
    /// Тянут ползунок блока боя - подставить образец боя в оба вывода.
    preview_fight: bool,
    /// Тянут ползунок подписи над врагом - показать образец в центре экрана.
    preview_tag: bool,
    /// Тянут положение строки босса - показать образцы у его полоски.
    preview_boss: bool,
    /// Идёт ли бой с боссом прямо сейчас. Снимается со снимка каждый кадр -
    /// покупки приходят до `collect`, и спрашивать игру в этот момент нельзя.
    boss_fight_now: bool,
    /// Игрок в Крепости Круглого стола. Снимается тем же способом и по той же
    /// причине, что и `boss_fight_now`.
    in_hub_now: bool,
    /// Идёт ли обычная игра - не меню, не катсцена, не загрузка. Считается по
    /// прошлому кадру: покупки вычерпываются до `collect`, а нажимать клавиши
    /// в инвентаре нельзя - это выбросит предмет вместо шага вперёд.
    gameplay_active: bool,
    /// Был ли игрок мёртв в прошлом кадре - смерть снимает эффекты по фронту.
    was_dead: bool,
    /// Когда зритель последний раз помешал играть: нажал клавишу или включил
    /// вредный эффект. Смерть в ближайшие `VIEWER_BLAME_WINDOW` считается его
    /// работой - см. `viewer_to_blame`.
    viewer_meddled_at: Option<std::time::Instant>,
    /// Награда, которую попросили проверить кнопкой, вместе с id уже
    /// заведённой для неё карточки. Исполняется не сразу, а когда игра снова
    /// примет ввод: пока открыто окно настроек, его забирает наш же детур, и
    /// нажатие до игры просто не дойдёт.
    ///
    /// Id тот же, что у карточки (`push_purchase_at`), а не пустая строка:
    /// пустой `redemption_id` фильтруется из `revert()`/`abandon_all()`, и
    /// «Снять эффекты» или уход из геймплея не могли снять тестовую карточку
    /// вообще (жалоба 2026-08-24). `reward_id` при этом остаётся пустым - им
    /// `refund`/`fulfill` и так отсекают тестовые покупки от сети.
    pending_test: Option<(twitch::rewards::Action, String)>,
    /// Кто сейчас в чате - из этого списка раздаются никнеймы врагам.
    /// Приезжает событием `ChattersUpdated`, своего `Arc` для этого не нужно.
    viewers: Vec<String>,
    /// Растёт на каждый новый список. Дешёвый способ сказать «состав сменился»
    /// вместо сравнения тысяч строк каждый кадр.
    viewers_gen: u64,
    /// Канал, для которого поднят чат-поток. Смена в настройках поднимает
    /// новый поток и обнуляет список: зрители прошлого канала над врагами -
    /// это чужие люди, а раньше они там и оставались (жалоба).
    chat_channel: Option<String>,
    /// Когда Helix последний раз прислал полный список зрителей. Пока он
    /// свежий, обновления из анонимного чата игнорируются: там только те, кто
    /// писал, и они затирали бы полный список неполным.
    helix_chatters_at: Option<std::time::Instant>,
    /// Команды сетевому потоку: возвраты баллов и создание наград.
    twitch_commands: twitch::Commands,
    /// До какого момента награда в перезарядке и чем её включать обратно на
    /// Twitch. Ключ - `RewardEntry::id`, значение - `(дедлайн, reward_id)`.
    /// Пока идёт перезарядка, награда на дашборде выключена, поэтому UUID
    /// хранится рядом со сроком: по истечении включать нечем иначе.
    reward_cooldown: std::collections::HashMap<u32, (std::time::Instant, String)>,
    /// Сколько тегов врагов игра показывает прямо сейчас - видно в
    /// диагностическом окне.
    native_tags_written: usize,
    /// Готовые подписи на этот кадр: считаются до проверок видимости панели,
    /// рисуются после неё.
    /// Ник, его последняя реплика в чате (если включено «последнее слово») и
    /// позиция на экране.
    enemy_tags: Vec<(String, Option<(String, f32)>, [f32; 2])>,
    /// У какой награды сейчас ждём нажатия клавиши.
    capture_key_for: Option<u32>,
    /// Что сообщить про создание наград (последняя ошибка).
    reward_notice: Option<String>,
    /// Кому какой ник достался. Держится между кадрами, иначе имена
    /// мельтешили бы.
    nicknames: enemies::NicknameAssigner,
    /// Было ли подключение к Twitch на прошлом кадре. По переходу
    /// «не было -> есть» разбираем хвост покупок, пришедших пока мы молчали.
    twitch_was_connected: bool,
    /// Связь хоть раз поднималась за эту сессию - чтобы первое подключение не
    /// объявлялось «восстановлением».
    twitch_ever_connected: bool,
}

/// Сколько подряд состояние должно выглядеть показуемым, чтобы HUD проявился.
/// Гаснет он сразу - на загрузках `menu_open` и флаг катсцены пропадают на
/// кадр-другой, и без этой задержки оба вывода мигали бы в каждую такую щель.
const SHOW_DEBOUNCE_SECS: f32 = 0.15;

/// Сколько покупок держим для окна настроек. На экране висят единицы, список в
/// настройках - короткая история «что вообще происходило».
const PURCHASE_HISTORY: usize = 30;

impl StreamHud {
    fn new(hmodule: usize, config: Config) -> Self {
        let (twitch_tx, twitch_rx) = std::sync::mpsc::channel();
        Self {
            hmodule,
            collector: Collector::new(hmodule),
            shared: Arc::new(Mutex::new(Shared {
                snapshot: Snapshot::default(),
                config: config.clone(),
                twitch: web::TwitchWeb::default(),
            })),
            font_signature: String::new(),
            font_reload_pending: false,
            web_started: false,
            frames: 0,
            // С нуля, а не с единицы: на первых кадрах сессии данные уже
            // читаются, а флагов ещё нет, и панель успевала мигнуть.
            hud_fade: 0.0,
            valid_for: 0.0,
            showable_for: 0.0,
            settings_open: false,
            settings_fade: 0.0,
            panel_drag: None,
            twitch_started: false,
            twitch_status: Arc::new(Mutex::new(twitch::Status::default())),
            twitch_rx: Mutex::new(twitch_rx),
            twitch_tx,
            purchases: Vec::new(),
            purchase_log: std::collections::VecDeque::new(),
            purchases_dirty: false,
            test_purchases: 0,
            rewards: twitch::rewards::load(hmodule),
            actions: twitch::actions::ActionState::default(),
            spawn: spawn::SpawnState::default(),
            effects: effects::EffectState::default(),
            boss_fight_now: false,
            in_hub_now: false,
            preview_toast: false,
            preview_fight: false,
            preview_tag: false,
            preview_boss: false,
            gameplay_active: false,
            was_dead: false,
            viewer_meddled_at: None,
            pending_test: None,
            viewers: Vec::new(),
            viewers_gen: 0,
            chat_channel: None,
            helix_chatters_at: None,
            twitch_commands: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            reward_cooldown: std::collections::HashMap::new(),
            native_tags_written: 0,
            enemy_tags: Vec::new(),
            capture_key_for: None,
            reward_notice: None,
            nicknames: enemies::NicknameAssigner::default(),
            twitch_was_connected: false,
            twitch_ever_connected: false,
            config,
        }
    }

    /// Поднимает сетевой поток Twitch и вычерпывает то, что он прислал.
    ///
    /// Условие запуска, в отличие от веба, не ждёт `snapshot.valid`: соединение
    /// не зависит от игровой памяти, а держать его закрытым, пока стример
    /// стоит в главном меню, значит лишить зрителей возможности что-то купить.
    fn pump_twitch(&mut self) {
        // Галочка действует на живой поток, а не только на его запуск: без
        // этого выключенная интеграция продолжала бы держать соединение и
        // исполнять покупки до перезапуска игры.
        twitch::set_enabled(self.config.twitch_enabled);
        // Client ID тоже живой, а не снимок на момент запуска потока: его
        // подтверждают кнопкой в окне настроек, и «перезапусти игру» в ответ на
        // исправленную опечатку - плохой ответ. Пустой поток обрабатывает сам
        // (`Status::NotConfigured`).
        twitch::set_client_id(self.config.twitch_client_id.trim());
        if self.config.twitch_enabled && !self.twitch_started {
            self.twitch_started = true;
            twitch::spawn(
                self.hmodule,
                twitch::SCOPE_REDEMPTIONS.to_string(),
                Arc::clone(&self.twitch_status),
                self.twitch_tx.clone(),
                Arc::clone(&self.twitch_commands),
            );
        }

        // Чат поднимается отдельно от авторизации: он анонимный, и работает
        // даже когда Client ID не введён вовсе. Поэтому и условие своё - имя
        // канала, а не `twitch_enabled`.
        //
        // Сравниваем каждый кадр, а не поднимаем один раз за сессию: канал
        // меняют прямо в окне настроек, и старый список зрителей после этого
        // не имеет к нему никакого отношения.
        let channel = twitch::chat::channel_name(&self.config.twitch_channel);
        if channel != self.chat_channel {
            self.chat_channel = channel.clone();
            self.viewers.clear();
            self.viewers_gen = self.viewers_gen.wrapping_add(1);
            self.helix_chatters_at = None;
            // Сетевому потоку канал нужен, чтобы решить, спрашивать ли полный
            // список зрителей: Twitch отдаёт его только модератору канала.
            twitch::set_channel(channel.as_deref().unwrap_or_default());
            match channel {
                Some(name) => twitch::chat::spawn(name, self.twitch_tx.clone()),
                None => twitch::chat::stop(),
            }
        }

        // Подключились после молчания - разобрать хвост. Пока мод не слушал,
        // покупки копились у Twitch в статусе «не выполнено», и сам он их
        // потом не переигрывает: без этого зритель платит в пустоту.
        let status = self.twitch_status.lock().map(|s| s.clone()).unwrap_or_default();
        let connected = matches!(status, twitch::Status::Connected { .. });
        if connected && !self.twitch_was_connected {
            self.sweep_unfulfilled();
            // Награда, выключенная под перезарядку, включается обратно по её
            // истечении - но если игра вылетела посреди неё, включать было
            // некому. Поэтому на каждом подключении возвращаем на дашборд всё,
            // что мод считает включённым.
            //
            // ponytail: этим же отменяется ручное «Выключить все на Twitch» -
            // до следующего подключения, а не навсегда. Названо в подсказке
            // под кнопкой.
            self.enable_all_rewards(true);
        }
        if let Some(text) = connection_notice(self.twitch_was_connected, self.twitch_ever_connected, &status) {
            self.push_notice(text);
        }
        self.twitch_ever_connected |= connected;
        self.twitch_was_connected = connected;

        // Неблокирующе: игровой поток не должен ждать сеть ни кадра.
        let drained: Vec<twitch::Event> = match self.twitch_rx.lock() {
            Ok(rx) => rx.try_iter().collect(),
            Err(_) => return,
        };
        let arrived = !drained.is_empty();
        for event in drained {
            match event {
                twitch::Event::Redemption {
                    viewer,
                    reward_id,
                    reward_title,
                    cost,
                    redemption_id,
                } => self.redeem(viewer, reward_id, reward_title, cost, redemption_id),
                twitch::Event::ChattersUpdated { list, helix } => {
                    // Источник выбран руками - чужой не берём вовсе. Запросы
                    // при этом продолжают идти обоими путями: чат нужен ещё и
                    // ради «последнего слова», а Helix спрашивается раз в
                    // минуту и почти ничего не стоит.
                    // ponytail: фильтр на приёме, а не выключение источника.
                    match self.config.viewers_source {
                        config::ViewerSource::App if !helix => continue,
                        config::ViewerSource::Chat if helix => continue,
                        _ => {}
                    }
                    // Полный список из Helix главнее: в чате видно только тех,
                    // кто писал. Пока он свежий, обновления из чата пропускаем.
                    const HELIX_FRESH: Duration = Duration::from_secs(300);
                    let helix_fresh =
                        self.helix_chatters_at.is_some_and(|at| at.elapsed() < HELIX_FRESH);
                    if !helix && helix_fresh {
                        continue;
                    }
                    if helix {
                        self.helix_chatters_at = Some(std::time::Instant::now());
                    }
                    // Поколение двигаем вместе со списком: по нему
                    // `NicknameAssigner` понимает, что пора сверяться заново.
                    self.viewers = list;
                    self.drop_blocked();
                    self.viewers_gen = self.viewers_gen.wrapping_add(1);
                }
                twitch::Event::RewardCreated { local_id, reward_id, mark } => {
                    // Запоминаем id: по нему покупки сопоставляются точно, даже
                    // если стример потом переименует награду.
                    if let Some(e) = self.rewards.iter_mut().find(|e| e.id == local_id) {
                        e.reward_id = reward_id;
                        e.synced = mark;
                        twitch::rewards::save(self.hmodule, &self.rewards);
                        self.reward_notice = Some(i18n::t("the reward was created on Twitch").to_string());
                    }
                }
                twitch::Event::RewardUpdated { local_id, mark } => {
                    let mut name = String::new();
                    if let Some(e) = self.rewards.iter_mut().find(|e| e.id == local_id) {
                        // Отпечаток именно отправленный: пока запрос летел,
                        // цену могли поправить ещё раз, и метка «совпадает»
                        // была бы враньём.
                        e.synced = mark;
                        name = e.reward_title.trim().to_string();
                        twitch::rewards::save(self.hmodule, &self.rewards);
                    }
                    self.reward_notice =
                        Some(format!("{} {name}", i18n::t("updated on Twitch:")));
                }
                twitch::Event::RewardsSynced { marks } => {
                    // Метка ставится по тому, что РЕАЛЬНО лежит на Twitch, а
                    // не по тому, что мы туда отправили: PATCH мог не
                    // примениться (награду завели другим приложением, её
                    // удалили с дашборда), и «изменено» висело бы вечно
                    // (жалоба 2026-08-23). Награду, которой в ответе нет,
                    // не трогаем: её просто не видно этому приложению.
                    let mut dirty = false;
                    for e in self.rewards.iter_mut().filter(|e| !e.reward_id.is_empty()) {
                        if let Some((_, mark)) = marks.iter().find(|(id, _)| *id == e.reward_id) {
                            dirty |= e.synced != *mark;
                            e.synced = *mark;
                        }
                    }
                    if dirty {
                        twitch::rewards::save(self.hmodule, &self.rewards);
                    }
                }
                twitch::Event::RefundFailed { reason } => self.reward_notice = Some(reason),
                twitch::Event::Notice { text } => self.reward_notice = Some(text),
                twitch::Event::RewardFailed { local_id, reason } => {
                    let title = self
                        .rewards
                        .iter()
                        .find(|e| e.id == local_id)
                        .map(|e| e.reward_title.clone())
                        .unwrap_or_default();
                    self.reward_notice = Some(format!("{title}: {reason}"));
                }
            }
        }

        // Перезарядка кончилась - возвращаем награду на дашборд. Пока она
        // идёт, награда там выключена, и без этого шага она осталась бы
        // выключенной навсегда.
        for (_, reward_id) in expired_cooldowns(&mut self.reward_cooldown, std::time::Instant::now()) {
            self.enable_reward(&reward_id, true);
        }

        // Догоревшие карточки выбрасываем, а не оставляем лежать до вытеснения
        // новыми: иначе после первой же покупки мод до конца сессии каждый кадр
        // копирует этот список в `Shared` и заходит в отрисовку впустую.
        //
        // Срок берём у самой карточки (`PurchaseEvent::alive`): у временного
        // эффекта он свой, и общий его тут обрезал - «таймер идёт, а карточки
        // уже нет» (жалоба 2026-08-24).
        let shared = self.config.twitch_notify_secs.max(0.0);
        let before = self.purchases.len();
        self.purchases.retain(|p| p.alive(shared));
        // Публикуем в `Shared` только когда список правда изменился - это
        // клонирование строк, а не пара чисел.
        self.purchases_dirty |= arrived || self.purchases.len() != before;
    }

    /// Покупка в ленту показа. Один путь и для настоящего события, и для
    /// тестового: иначе кнопка проверки показывала бы не то, что покажет
    /// живой Twitch.
    fn push_purchase(&mut self, viewer: String, label: String, cost: u32) {
        self.push_purchase_at(viewer, label, cost, None, None, String::new());
    }

    /// Состояние мода той же карточкой, что и покупка: пропавшую связь иначе
    /// видно только в окне настроек, а во время стрима туда не смотрят.
    ///
    /// Цена нулевая - платить было некому, и справа она не рисуется. Слияние
    /// по подписи (`upsert_purchase`) тут кстати: повторное «связь потеряна»
    /// обновляет ту же карточку, а не строит вторую.
    fn push_notice(&mut self, text: &str) {
        self.push_purchase("Twitch".to_string(), text.to_string(), 0);
    }

    /// То же, но со сроком жизни карточки и моментом, до которого она считает
    /// обратный отсчёт - см. `card_timing`.
    fn push_purchase_at(
        &mut self,
        viewer: String,
        label: String,
        cost: u32,
        countdown_to: Option<std::time::Instant>,
        life: Option<f32>,
        redemption_id: String,
    ) {
        upsert_purchase(
            &mut self.purchases,
            PURCHASE_HISTORY,
            twitch::PurchaseEvent {
                viewer,
                label,
                cost,
                at: std::time::Instant::now(),
                countdown_to,
                life,
                redemption_id,
            },
        );
        self.purchases_dirty = true;
    }

    /// Покупка приехала: найти её в списке наград и сделать, что велено.
    ///
    /// Награда, которой в списке нет, всё равно показывается карточкой - это
    /// не ошибка, а обычное дело: у стримера на канале могут быть награды, не
    /// имеющие отношения к игре.
    #[allow(clippy::too_many_arguments)]
    /// Выкидывает из списка тех, кого стример не хочет видеть над врагами.
    ///
    /// Чистим сам список, а не фильтруем на каждом кадре: зрителей бывает две
    /// тысячи, а раздача имён идёт из `assign` каждый кадр с тегами.
    fn drop_blocked(&mut self) {
        if self.config.viewer_block.is_empty() {
            return;
        }
        self.viewers.retain(|v| !self.config.viewer_block.contains(&v.to_lowercase()));
    }

    /// «Обновить»: начать перебор зрителей с нуля.
    ///
    /// Забываем и список, и кто какое имя уже носил (`NicknameAssigner` с его
    /// кулдаунами), и перезапускаем чат - он пришлёт состав заново. Helix
    /// спросится сам в свою минуту; торопить его отсюда нечем - счётчик живёт
    /// в сетевом потоке.
    fn refresh_viewers(&mut self) {
        self.viewers.clear();
        self.viewers_gen = self.viewers_gen.wrapping_add(1);
        self.helix_chatters_at = None;
        self.nicknames = enemies::NicknameAssigner::default();
        twitch::CHATTERS_DENIED.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(name) = self.chat_channel.clone() {
            twitch::chat::spawn(name, self.twitch_tx.clone());
        }
    }

    fn redeem(
        &mut self,
        viewer: String,
        reward_id: String,
        reward_title: String,
        cost: u32,
        redemption_id: String,
    ) {
        let found = twitch::rewards::find_match(&self.rewards, &reward_id, &reward_title);
        // Своя награда, но снятая галочкой: мод её знает и осознанно не
        // исполняет - значит и держать за неё баллы не за что. Чужую (которой
        // в списке нет вовсе) не трогаем, у стримера могут быть свои награды.
        let known_but_off = found.is_some_and(|e| !e.enabled);
        let matched = found.filter(|e| e.enabled).cloned();

        let label = match &matched {
            Some(e) => e.caption().to_string(),
            None => reward_title,
        };
        // Копия до `push_purchase`, который забирает ник себе: спавну он нужен,
        // чтобы подписать заспавненного врага именем покупателя.
        let viewer_name = viewer.clone();
        // Ник копируется тут же: он сейчас уедет в карточку, а в журнал
        // попадёт только если покупка состоится.
        let viewer_logged = viewer.clone();
        let (life, countdown_to) =
            card_timing(matched.as_ref().map(|e| e.action), self.config.twitch_notify_secs);
        self.push_purchase_at(viewer, label.clone(), cost, countdown_to, life, redemption_id.clone());

        // Награды, которой нет в списке, мод не касается: у стримера на канале
        // могут быть свои, к игре не относящиеся. Возвращать за них баллы не
        // наше дело.
        let Some(entry) = matched else {
            if known_but_off {
                self.refund(&reward_id, &redemption_id);
            }
            return;
        };

        // Не в игре - меню, катсцена, загрузка, открытое окно настроек или
        // вообще другое окно на переднем плане. Клавишу тут нажимать нельзя (в
        // инвентаре она выбросит предмет, а вне фокуса уйдёт в чужое
        // приложение), а копить покупку до возвращения значит исполнить её
        // неизвестно когда, когда зритель уже забыл, за что платил.
        if !self.gameplay_active {
            self.refund(&reward_id, &redemption_id);
            return;
        }

        // Крепость Круглого стола - хаб: там торгуют, качаются и разговаривают
        // с NPC, а не дерутся. Купленная клавиша попадёт в диалог, а спавн
        // враждебного существа в единственное безопасное место в игре - это не
        // то, за что зритель платил.
        if self.in_hub_now {
            self.reward_notice = Some(i18n::t("The player is in Roundtable Hold - purchases are not executed there.")
            .to_string());
            self.refund(&reward_id, &redemption_id);
            return;
        }

        // Бой с боссом: у каждой награды-эффекта своя галочка. Проверяем до
        // очереди - покупка, которую всё равно не исполнить, не должна занимать
        // в ней место.
        if self.boss_fight_now && !entry.action.allowed_in_boss() {
            self.reward_notice = Some(
                i18n::t("Boss fight in progress - this reward is disabled during one.")
                .to_string(),
            );
            self.refund(&reward_id, &redemption_id);
            return;
        }

        let now = std::time::Instant::now();
        // Перезарядка самой НАГРАДЫ - единственная в моде. Прежние «паузы на
        // зрителя» по категориям были тем же самым с другой стороны и удалены
        // как дубль (запрос 2026-09-03).
        //
        // Пока она идёт, награда ВЫКЛЮЧЕНА и на Twitch, так что сюда обычно
        // не доходит вовсе. Проверка всё равно нужна: выключение - сетевой
        // запрос, и покупка, ушедшая до него, дойдёт до нас.
        let cooldown = Duration::from_secs(u64::from(entry.action.cooldown_secs()));
        if !cooldown.is_zero() {
            if let Some((until, _)) = self.reward_cooldown.get(&entry.id) {
                if now < *until {
                    let left = until.duration_since(now).as_secs() + 1;
                    self.reward_notice =
                        Some(format!("{}: {} {}", label, i18n::t("on cooldown, seconds left:"), left));
                    self.refund(&reward_id, &redemption_id);
                    return;
                }
            }
        }

        let pending = twitch::actions::Pending {
            action: entry.action,
            reward_id: reward_id.clone(),
            redemption_id: redemption_id.clone(),
            viewer: viewer_name,
        };
        // Очередь полна - возвращаем баллы сразу, а не копим покупки, до
        // которых дело дойдёт через минуту.
        if let Some(rejected) = self.actions.enqueue(pending, self.config.action_queue_limit as usize) {
            self.refund(&rejected.reward_id, &rejected.redemption_id);
            return;
        }
        // Журнал - только состоявшиеся покупки, поэтому пишется здесь, а не
        // рядом с карточкой: та заводится до всех проверок и при отказе тут же
        // стирается вместе с возвратом баллов.
        self.log_purchase(&viewer_logged, &label, cost);
        if !cooldown.is_zero() {
            self.reward_cooldown.insert(entry.id, (now + cooldown, reward_id.clone()));
            // Выключаем награду на дашборде: «не пропускать в игре» зритель
            // видит как молчаливый возврат баллов, а погашенная награда
            // объясняет сама себя.
            self.enable_reward(&reward_id, false);
        }
    }

    /// Строка в журнал «Последних покупок». Отдельно от карточки: та живёт
    /// секунды и чистится каждый кадр, а журнал держит 30 записей до конца
    /// сессии.
    fn log_purchase(&mut self, viewer: &str, label: &str, cost: u32) {
        self.purchase_log.push_back(format!("{viewer} \u{b7} {label} ({cost})"));
        while self.purchase_log.len() > PURCHASE_HISTORY {
            self.purchase_log.pop_front();
        }
    }

    /// Все свои награды разом - кнопки «Выключить все на Twitch» / «Включить
    /// все». Выключаем всё, что мод знает; включаем только то, что у него
    /// самого помечено галочкой.
    fn enable_all_rewards(&mut self, on: bool) {
        let ids: Vec<String> = self
            .rewards
            .iter()
            .filter(|e| !e.reward_id.is_empty() && (!on || e.enabled))
            .map(|e| e.reward_id.clone())
            .collect();
        for id in ids {
            self.enable_reward(&id, on);
        }
    }

    /// Включить или выключить награду на дашборде Twitch.
    ///
    /// Отдельно от `Command::UpdateReward`: тот `is_enabled` не шлёт намеренно
    /// (галочка мода значит «мод исполняет награду», а не «зрители её видят»),
    /// и слать его оттуда значило бы гасить награду при каждой правке цены.
    fn enable_reward(&mut self, reward_id: &str, on: bool) {
        // Разбирает очередь сетевой поток, а при снятой галочке он спит:
        // без этой проверки кнопки «Выключить/включить все» копили бы команды,
        // которые некому исполнить.
        if reward_id.is_empty() || !self.config.twitch_enabled {
            return;
        }
        if let Ok(mut q) = self.twitch_commands.lock() {
            q.push_back(twitch::Command::EnableReward {
                reward_id: reward_id.to_string(),
                on,
            });
        }
    }

    /// Виноват ли зритель в этой смерти. Два признака, и достаточно любого:
    ///
    /// - **в мире стоит присланный врагом гость** (`SpawnState::any_alive`);
    /// - **недавняя помеха**: клавиша или вредный эффект за последние
    ///   `VIEWER_BLAME_WINDOW`.
    ///
    /// Оба грубые, и это осознанно. Точный вопрос - «кто нанёс последний
    /// удар» - игра надёжно не отвечает: `ChrIns::last_hit_by` у выстрела и
    /// заклинания называет САМ СНАРЯД (то же самое разбиралось в руническом
    /// обмене). Первая версия счётчика стояла ровно на этом поле и живьём не
    /// сработала ни разу - босс дерётся не одними кулаками (жалоба
    /// 2026-08-25).
    ///
    /// Кнопка «Проверить» считается наравне с покупкой: иначе проверить
    /// счётчик нечем вовсе, а это первое, что с ним делают.
    fn viewer_to_blame(&self) -> bool {
        self.spawn.any_alive()
            || self.viewer_meddled_at.is_some_and(|at| at.elapsed() < VIEWER_BLAME_WINDOW)
    }

    /// Убирает карточки этих покупок с экрана и из виджета.
    ///
    /// Не возврат баллов: эффект успел поработать, платить за него зритель
    /// должен. Убираем только показ - висящий отсчёт до конца того, чего уже
    /// нет, врёт на весь стрим.
    fn drop_purchases(&mut self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let before = self.purchases.len();
        self.purchases.retain(|p| !ids.contains(&p.redemption_id));
        self.purchases_dirty |= self.purchases.len() != before;
    }

    /// Просит сетевой поток вернуть баллы за покупку.
    ///
    /// Пустой `redemption_id` - это тестовая покупка из окна настроек: там
    /// возвращать нечего.
    fn refund(&mut self, reward_id: &str, redemption_id: &str) {
        if redemption_id.is_empty() || reward_id.is_empty() {
            return;
        }
        // Баллы вернули - карточки «зритель купил» быть не должно: в игре
        // ничего не произошло, и деньги отданы обратно. Обычно это тот же
        // кадр, в котором карточку и завели, так что на экране она даже не
        // мелькнёт; отложенный спавн снимает её позже, досчитав до отказа.
        let before = self.purchases.len();
        self.purchases.retain(|p| p.redemption_id != redemption_id);
        self.purchases_dirty |= self.purchases.len() != before;
        if let Ok(mut q) = self.twitch_commands.lock() {
            q.push_back(twitch::Command::Refund {
                reward_id: reward_id.to_string(),
                redemption_id: redemption_id.to_string(),
            });
        }
    }

    /// Помечает покупку выполненной у Twitch.
    ///
    /// Не украшательство: пока покупка числится «не выполнено», разбор хвоста
    /// (`sweep_unfulfilled`) не может отличить её от той, что мод проспал, и
    /// вернул бы за неё баллы задним числом.
    fn fulfill(&mut self, reward_id: &str, redemption_id: &str) {
        if redemption_id.is_empty() || reward_id.is_empty() {
            return;
        }
        if let Ok(mut q) = self.twitch_commands.lock() {
            q.push_back(twitch::Command::Fulfill {
                reward_id: reward_id.to_string(),
                redemption_id: redemption_id.to_string(),
            });
        }
    }

    /// Просит сетевой поток вернуть баллы за всё, что мод проспал.
    ///
    /// `skip` - то, что прямо сейчас лежит в очередях: у Twitch эти покупки
    /// тоже «не выполнено», но они вот-вот исполнятся.
    fn sweep_unfulfilled(&mut self) {
        let rewards: Vec<String> =
            self.rewards.iter().filter(|e| !e.reward_id.is_empty()).map(|e| e.reward_id.clone()).collect();
        if rewards.is_empty() {
            return;
        }
        let mut skip: Vec<String> = self.actions.pending_ids().map(str::to_string).collect();
        skip.extend(self.spawn.pending_ids());
        if let Ok(mut q) = self.twitch_commands.lock() {
            q.push_back(twitch::Command::SweepUnfulfilled { rewards, skip });
        }
    }

    /// Общая точка входа для спавна врага - и по настоящей покупке из
    /// очереди, и по кнопке «Проверить» (тогда `reward_id`/`redemption_id`
    /// пустые, и `refund` на них и так ничего не делает).
    fn try_spawn(
        &mut self,
        action: twitch::rewards::Action,
        reward_id: String,
        redemption_id: String,
        viewer: String,
    ) {
        let twitch::rewards::Action::SpawnEnemy { key, .. } = action else { return };
        // Время жизни одно на все спавны и живёт в «Спавне врагов»: своё поле
        // у награды дублировало ту же настройку (жалоба 2026-09-03).
        let ttl = Duration::from_secs_f32(self.config.spawn_ttl_secs.max(0.1));
        let delay = Duration::from_secs(u64::from(action.spawn_delay()));
        // Бой с боссом: третий участник посреди попытки - это чаще испорченный
        // ран, чем веселье, поэтому решает стример. Проверяем до всего
        // остального - синглтон резолвить незачем.
        if self.config.spawn_block_in_boss && self.boss_fight_now {
            self.reward_notice = Some(spawn::SpawnRejected::BossFight.reason());
            self.refund(&reward_id, &redemption_id);
            return;
        }
        let outcome =
            self.spawn.schedule(
                key,
                delay,
                ttl,
                self.config.spawn_limit,
                self.config.spawn_same_limit,
                &reward_id,
                &redemption_id,
                &viewer,
            );
        // Лимит, неподобранный param-id, нет игрока - за неисполненное
        // возвращаем баллы сразу, а не молча проглатываем покупку. Причину
        // пишем в окно настроек: лога в моде нет, и без неё кнопка
        // «Проверить» просто ничего не делает, не объясняя почему.
        match outcome {
            Err(why) => {
                self.reward_notice = Some(why.reason());
                self.refund(&reward_id, &redemption_id);
            }
            // Иначе прошлая причина отказа висела бы в окне и после удачного
            // спавна, читаясь как «до сих пор не работает».
            Ok(()) => self.reward_notice = None,
        }
    }

    /// Общая точка входа для эффекта - и по покупке из очереди, и по кнопке
    /// «Проверить» (тогда `reward_id`/`redemption_id` пустые).
    ///
    /// Отдельно от `try_spawn`: у эффекта нет ни позиции, ни TTL в мире, ни
    /// покупателя над головой, и запрет «не в бою с боссом» ему не подходит
    /// вовсе - половина эффектов ради боя и покупается.
    fn try_effect(&mut self, action: twitch::rewards::Action, reward_id: String, redemption_id: String) {
        let twitch::rewards::Action::Effect { key, secs, .. } = action else { return };
        match self.effects.apply(key, secs, &redemption_id) {
            Err(why) => {
                self.reward_notice = Some(why.reason());
                self.refund(&reward_id, &redemption_id);
            }
            // Эффект применяется сразу и целиком - в отличие от спавна, ждать
            // подтверждения движка тут нечего.
            Ok(()) => {
                self.reward_notice = None;
                self.fulfill(&reward_id, &redemption_id);
            }
        }
    }
}

/// Сколько карточка покупки висит и до какого момента показывает отсчёт.
///
/// Одна функция на оба места, где карточка заводится - покупку и кнопку
/// «Проверить»: разъехавшись, они дали ровно то, на что и пожаловались -
/// проверяемый эффект идёт, а карточка уже пропала.
///
/// **Карточка с отсчётом никогда не уходит раньше своего отсчёта.** Это и
/// было той жалобой, и та же дыра нашлась разбором в соседней ветке: у спавна
/// с задержкой в минуту карточка жила общие несколько секунд и пропадала,
/// досчитав до 57. Поэтому срок считается ЗДЕСЬ для обоих случаев, а не
/// берётся общим по умолчанию.
///
/// - у временного эффекта карточка живёт весь его срок и считает до конца;
/// - у отложенного спавна - до появления врага плюс `shared` сверху, чтобы
///   было видно, что гость пришёл;
/// - у остального ни того, ни другого нет, и берётся общий срок показа.
fn card_timing(
    action: Option<twitch::rewards::Action>,
    shared: f32,
) -> (Option<f32>, Option<std::time::Instant>) {
    let now = std::time::Instant::now();
    match action {
        Some(twitch::rewards::Action::Effect { key, secs, .. }) => {
            let secs = if secs > 0 { secs } else { effects::default_secs(key) };
            // Тот же потолок, что и у самого эффекта (`EffectState::apply`):
            // в файле наград можно руками написать что угодно, и карточка
            // тогда пережила бы эффект, досчитывая до нуля впустую.
            let secs = secs.min(effects::MAX_SECS);
            match secs {
                0 => (None, None),
                s => (Some(f32::from(s)), Some(now + Duration::from_secs(u64::from(s)))),
            }
        }
        Some(twitch::rewards::Action::SpawnEnemy { delay_secs, .. }) if delay_secs > 0 => {
            let delay = f32::from(delay_secs);
            (Some(delay + shared.max(0.0)), Some(now + Duration::from_secs_f32(delay)))
        }
        _ => (None, None),
    }
}

/// Сколько после покупки-помехи смерть ещё считается работой зрителя.
const VIEWER_BLAME_WINDOW: Duration = Duration::from_secs(15);

/// Мешает ли эта покупка играть. Клавиша - всегда: зритель вмешался в
/// управление, чем бы это ни кончилось. Эффект - только вредный (`heal` и
/// фляги смерти не причина). Спавн сюда НЕ входит: у него признак точнее -
/// сам враг, который добил (см. `viewer_to_blame`).
fn meddles_with_play(action: &twitch::rewards::Action) -> bool {
    match action {
        twitch::rewards::Action::Press { .. } | twitch::rewards::Action::Hold { .. } => true,
        twitch::rewards::Action::Effect { key, .. } => effects::harmful(key),
        twitch::rewards::Action::SpawnEnemy { .. } => false,
    }
}

/// Что сказать карточкой на смене статуса подключения. `None` - молчим.
///
/// Свободная функция, а не метод: развилок три, и проверять их удобнее без
/// всего остального мода - тот же приём, что у `upsert_purchase`.
fn connection_notice(was: bool, ever: bool, now: &twitch::Status) -> Option<&'static str> {
    match (was, matches!(now, twitch::Status::Connected { .. })) {
        // Первое подключение и возврат после обрыва - разные новости.
        (false, true) if ever => Some(i18n::t("Twitch connection restored")),
        (false, true) => Some(i18n::t("Twitch connected")),
        // Выключенная галочка - не обрыв, а решение стримера: сообщать нечего.
        (true, false) if !matches!(now, twitch::Status::Disabled) => {
            Some(i18n::t("Twitch connection lost"))
        }
        _ => None,
    }
}

/// Кладёт покупку в ленту показа, сливая с уже висящей карточкой той же
/// награды - прямой запрос 2026-08-24: «одинаковые награды обновляют свою
/// карточку, а не добавляют новую». «Одинаковая» - по подписи (`label`): это
/// и есть то единственное, что отличает одну карточку от другой на экране.
///
/// Старую запись убираем и заводим заново в конце списка, а не правим на
/// месте - карточка ведёт себя как обычная новая покупка (свежий отсчёт,
/// обычное место среди последних), а не зависает в своей прежней
/// хронологической позиции.
///
/// Свободная функция, а не метод `StreamHud` - её можно проверить без всего
/// остального мода.
/// Перезарядки, которые истекли: убираем из карты и отдаём вызывающему, чтобы
/// он включил награды обратно на Twitch.
///
/// Свободной функцией, а не методом: развилка «истекла / ещё идёт» проверяется
/// без всего `StreamHud` (тот же приём, что у `upsert_purchase`).
fn expired_cooldowns(
    map: &mut std::collections::HashMap<u32, (std::time::Instant, String)>,
    now: std::time::Instant,
) -> Vec<(u32, String)> {
    let done: Vec<(u32, String)> = map
        .iter()
        .filter(|(_, (until, _))| now >= *until)
        .map(|(id, (_, reward))| (*id, reward.clone()))
        .collect();
    for (id, _) in &done {
        map.remove(id);
    }
    done
}

fn upsert_purchase(purchases: &mut Vec<twitch::PurchaseEvent>, cap: usize, event: twitch::PurchaseEvent) {
    purchases.retain(|p| p.label != event.label);
    purchases.push(event);
    if purchases.len() > cap {
        purchases.remove(0);
    }
}

#[cfg(test)]
mod purchase_upsert_tests {
    use super::*;

    /// Перезарядка кончилась - награда называется один раз и уходит из карты.
    /// Иначе мод слал бы «включить» на Twitch каждый кадр до конца сессии.
    #[test]
    fn an_expired_cooldown_is_reported_once() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let mut map = std::collections::HashMap::new();
        map.insert(1, (now - Duration::from_secs(1), "done".to_string()));
        map.insert(2, (now + Duration::from_secs(30), "running".to_string()));

        let out = expired_cooldowns(&mut map, now);
        assert_eq!(out, vec![(1, "done".to_string())]);
        assert!(expired_cooldowns(&mut map, now).is_empty());
        // Идущая перезарядка на месте: её награда остаётся выключенной.
        assert!(map.contains_key(&2));
    }

    /// «Смерти от зрителей» по таймеру: клавиша и вредный эффект её заводят,
    /// подарок и спавн - нет (у спавна свой, точный признак - кто добил).
    #[test]
    fn only_meddling_starts_the_blame_window() {
        use hudhook::imgui::Key;
        use twitch::rewards::Action;
        assert!(meddles_with_play(&Action::Press { key: Key::Space }));
        assert!(meddles_with_play(&Action::Hold { key: Key::W, duration_ms: 500 }));

        let effect = |key| Action::Effect { key, secs: 0, in_boss: true, cooldown_secs: 0 };
        assert!(meddles_with_play(&effect("flask_lock")), "забрали фляги - помеха");
        assert!(!meddles_with_play(&effect("heal")), "лечение смерти не причина");
        assert!(
            !meddles_with_play(&Action::SpawnEnemy { key: "x", delay_secs: 0, cooldown_secs: 0 }),
            "спавн считается по тому, кто добил, а не по времени"
        );
    }

    /// Карточка статуса появляется на нужных переходах и молчит на остальных.
    #[test]
    fn connection_notices_fire_only_on_a_real_change() {
        let on = twitch::Status::Connected { login: "me".into() };
        let err = twitch::Status::Error { message: "boom".into(), retry_in_secs: 5 };

        assert!(connection_notice(false, false, &on).is_some(), "первое подключение");
        let first = connection_notice(false, false, &on);
        let again = connection_notice(false, true, &on);
        assert_ne!(first, again, "возврат после обрыва - другая новость");

        assert!(connection_notice(true, false, &err).is_some(), "обрыв");
        assert!(connection_notice(true, false, &twitch::Status::Disabled).is_none(), "галочку сняли сами");
        assert!(connection_notice(true, true, &on).is_none(), "связь как была");
        assert!(connection_notice(false, true, &err).is_none(), "и не было, и нет");
    }

    fn card(label: &str, viewer: &str, cost: u32) -> twitch::PurchaseEvent {
        twitch::PurchaseEvent {
            viewer: viewer.to_string(),
            label: label.to_string(),
            cost,
            at: std::time::Instant::now(),
            countdown_to: None,
            life: None,
            redemption_id: String::new(),
        }
    }

    /// Разные награды копятся как есть - сливать нечего.
    #[test]
    fn different_labels_both_stay() {
        let mut list = Vec::new();
        upsert_purchase(&mut list, 30, card("Прыжок", "alpha", 100));
        upsert_purchase(&mut list, 30, card("Эффект", "beta", 200));
        assert_eq!(list.len(), 2);
    }

    /// Та же награда - одна карточка, с данными ПОСЛЕДНЕЙ покупки, и она
    /// теперь последняя в списке (свежая позиция, а не старая).
    #[test]
    fn same_label_merges_into_one_with_latest_data() {
        let mut list = Vec::new();
        upsert_purchase(&mut list, 30, card("Прыжок", "alpha", 100));
        upsert_purchase(&mut list, 30, card("Другое", "gamma", 50));
        upsert_purchase(&mut list, 30, card("Прыжок", "beta", 150));
        assert_eq!(list.len(), 2, "одинаковая подпись обязана слиться в одну карточку");
        let jump = list.last().expect("последняя запись - это только что слитая");
        assert_eq!(jump.label, "Прыжок");
        assert_eq!(jump.viewer, "beta", "показывать должны того, кто купил последним");
        assert_eq!(jump.cost, 150);
    }

    /// Потолок истории по-прежнему режет самое старое, слияние его не обходит.
    #[test]
    fn cap_still_trims_the_oldest() {
        let mut list = Vec::new();
        for i in 0..5 {
            upsert_purchase(&mut list, 3, card(&format!("Награда {i}"), "v", 10));
        }
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].label, "Награда 2", "первые две обязаны были выпасть");
    }
}

impl ImguiRenderLoop for StreamHud {
    fn initialize(&mut self, ctx: &mut Context, _render: &mut dyn RenderContext) {
        overlay::load_fonts(ctx, &self.config);
        self.font_signature = self.config.font_signature();
        // Без своего обработчика Ctrl+V в полях ввода молча не работает:
        // imgui-rs не ставит его сам, а игра ничего не предоставляет.
        ctx.set_clipboard_backend(input::Clipboard);
    }

    /// Единственное место, где доступны и `Context`, и `RenderContext`, - здесь
    /// и только здесь можно перестроить атлас.
    ///
    /// Безопасно только потому, что нигде не хранится `FontId`: каждый вызов
    /// отрисовки берёт шрифт по индексу атласа через `font_at()`.
    fn before_render(&mut self, ctx: &mut Context, render: &mut dyn RenderContext) {
        if !self.font_reload_pending {
            return;
        }
        self.font_reload_pending = false;

        // Перезагрузка запрашивается на любое изменение конфига, но атлас
        // зависит только от пути к шрифту и размеров. Пропуск, когда ничего
        // шрифтового не поменялось, не даёт F6 копить атласы впустую.
        let signature = self.config.font_signature();
        if signature == self.font_signature {
            return;
        }
        self.font_signature = signature;

        ctx.fonts().clear();
        overlay::load_fonts(ctx, &self.config);

        // Куча текстур в hudhook 0.9.2 - это `Vec` без удаления, поэтому новая
        // текстура на каждую пересборку утекает до конца сессии. Один атлас на
        // размер: совпал - переиспользуем (`replace_texture` требует точного
        // совпадения размеров), не совпал - заводим новую.
        let fonts = ctx.fonts();
        let current = fonts.tex_id;
        let texture = fonts.build_rgba32_texture();
        let (w, h) = (texture.width, texture.height);
        if render.replace_texture(current, texture.data, w, h).is_err() {
            if let Ok(id) = render.load_texture(texture.data, w, h) {
                fonts.tex_id = id;
            }
        }
    }

    /// Пока окно настроек открыто, оконные сообщения ввода до `wnd_proc` игры
    /// не доходят. Сам ImGui их всё равно получает от hudhook, так что F7
    /// закроет окно. Основную работу делают детуры в `input`: игра читает ввод
    /// через raw input, а не через очередь сообщений (измерено в elden).
    fn message_filter(&self, _io: &hudhook::imgui::Io) -> hudhook::MessageFilter {
        if self.settings_open {
            hudhook::MessageFilter::InputKeyboard | hudhook::MessageFilter::InputMouse
        } else {
            hudhook::MessageFilter::empty()
        }
    }

    fn render(&mut self, ui: &mut Ui) {
        // Баг hudhook 0.9.2, при любом режиме экрана:
        // `update_display_size_from_swap_chain` ставит `display_size` в размер
        // бэкбуфера (это уже пиксели фреймбуфера) И `display_framebuffer_scale`
        // в `GetDpiForWindow/96`, а DX12 потом их перемножает. При масштабе
        // Windows 150% выходит вьюпорт 2880x1620 поверх бэкбуфера 1920x1080:
        // всё в полтора раза больше, HUD уезжает вправо-вниз.
        //
        // Должно стоять именно здесь, а не в `before_render`: между ними
        // hudhook перезапускает свою функцию и перезаписывает scale, а читает
        // его сразу после возврата отсюда.
        //
        // ponytail: одна строка против форка hudhook; убрать, если починят.
        unsafe {
            (*hudhook::imgui::sys::igGetIO()).DisplayFramebufferScale =
                hudhook::imgui::sys::ImVec2 { x: 1.0, y: 1.0 }
        };

        self.frames += 1;

        // Список боссов открывается своей клавишей: лезть за ним в настройки
        // посреди боя неудобно, а окно живёт само по себе.
        if ui.is_key_pressed(self.config.boss_list_key) {
            settings::toggle_boss_list();
        }

        if ui.is_key_pressed(self.config.settings_key) {
            self.settings_open = !self.settings_open;
            if self.settings_open {
                input::release_cursor_clip();
            } else {
                input::release_capture();
            }
        }

        // До аварийного выключателя: Twitch - это вход, а не вывод. Покупки
        // продолжают приходить и попадать в историю, даже когда обе панели
        // погашены, и статус в окне настроек остаётся живым.
        self.pump_twitch();

        // Аварийный выключатель: оба вывода выключены - значит и читать нечего.
        // Способ погасить мод, не вынимая DLL из загрузчика. Не должен
        // запирать открытое окно настроек: иначе выключить оба вывода,
        // оставив только диагностику, значит навсегда потерять доступ к
        // F7 - переключать их обратно было бы уже нечем (найдено по жалобе).
        if !self.config.overlay_enabled && !self.config.web_enabled && !self.config.debug && !self.settings_open {
            // Отсюда `tick` уже не вызывается, а значит зажатую зрителем
            // клавишу отпустить будет некому - она осталась бы нажатой во всей
            // системе до конца сессии. Купленное и не исполненное возвращаем.
            self.actions.release_all();
            for left in self.actions.drain_queue() {
                self.refund(&left.reward_id, &left.redemption_id);
            }
            for (rid, red) in self.spawn.abandon_all() {
                self.refund(&rid, &red);
            }
            // Тикать станет некому, а замедленная игра сама не ускорится.
            let cancelled = self.effects.revert_all();
            self.drop_purchases(&cancelled);
            self.gameplay_active = false;
            return;
        }

        // Читаем игровую память отсюда: render loop идёт на игровом потоке,
        // это самое безопасное место для этого.
        let mut snapshot = self.collector.collect();
        // Ближайший босс - из того же кэша, что и окно списка: он обновляется
        // раз в секунду, поэтому звать можно каждый кадр. Прячем на время боя и
        // ещё пока висит строка «СМЕРТЕЙ · время» убитого босса (`boss_name`
        // держится весь `BOSS_LINGER`): важен тот, что перед тобой, а не
        // следующий по маршруту.
        if self.config.show_nearest_boss
            && !snapshot.boss_fight_active
            && snapshot.boss_name.is_none()
        {
            snapshot.nearest_boss = bosses::nearest(self.config.boss_list_radius);
        }

        // Имя босса, с которым идёт бой, игра рисует сама - связываем его со
        // строкой парама, у которой имени не нашлось. Иначе боссы без маркера
        // на карте и без благодати (те же драконы открытого мира) в списке не
        // появятся никогда.
        if let Some(names) = snapshot.boss_name.as_deref() {
            let names: Vec<String> = names.lines().map(str::to_string).collect();
            bosses::learn(&names);
        }

        // Прогрев. **Жалоба 2026-08-20: при запуске игры и на загрузках HUD
        // мигает - показывается на мгновение и прячется.** Причина в порядке:
        // память становится читаемой (`valid`) раньше, чем игра выставляет
        // `menu_open` и флаг катсцены, поэтому кадр-другой состояние выглядит
        // как «идёт обычная игра». Ждём, пока данные продержатся подряд
        // `WARMUP_SECS` - за это время флаги успевают догнать.
        //
        // Пока идёт обычная игра, счётчик давно за потолком, поэтому выход из
        // инвентаря панель по-прежнему возвращает сразу.
        const WARMUP_SECS: f32 = 1.5;
        let frame_dt = ui.io().delta_time.clamp(0.0, 0.1);
        self.valid_for = if snapshot.valid { self.valid_for + frame_dt } else { 0.0 };
        let warmed = self.valid_for >= WARMUP_SECS;

        // Веб поднимаем после первого успешного чтения, а не в `DllMain`:
        // порт занимается на всю сессию, и делать это до того, как стало
        // понятно, что мод вообще работает, незачем.
        if self.config.web_enabled && !self.web_started && snapshot.valid {
            self.web_started = true;
            web::spawn(Arc::clone(&self.shared), self.config.web_port);
        }

        // Публикуем только когда есть кому читать: `Shared` существует ради
        // веб-выхода, и при выключенном ничего копировать не надо.
        if self.config.web_enabled {
            // Статус читаем ДО захвата `shared`: два мьютекса подряд, а не
            // вложенно - так порядок захвата не с чем спутать.
            let connected = matches!(
                self.twitch_status.lock().as_deref(),
                Ok(twitch::Status::Connected { .. })
            );
            // Клон готовим ДО лока: строки снимка копируются на каждом кадре, и
            // делать это с захваченным мьютексом значит держать на нём веб-поток.
            // Виджету в OBS уезжает тот же прогрев: страница прячется по
            // `valid`, и без этого она мигала бы ровно так же, как панель.
            let mut published = snapshot.clone();
            // Виджету в OBS уезжает не только прогрев, но и дребезг-фильтр:
            // страница гасит себя по `valid` тем же плавным переходом, что и
            // по `hidden`, а щель в `menu_open` на загрузке длиннее одного
            // кадра и в опрос раз в 250 мс попадает (жалоба 2026-09-03).
            // Раньше сюда ехал только `warmed`, и виджет мигал там, где панель
            // уже нет.
            published.valid =
                published.valid && warmed && self.showable_for >= SHOW_DEBOUNCE_SECS;
            // Тянут ползунок кегля имени босса или попытки: вне боя этих строк
            // нет, и настраивались бы они вслепую. Флаг с прошлого кадра -
            // окно настроек рисуется ниже, а страницу опрашивают раз в 250 мс.
            if self.preview_fight {
                demo_fight(&mut published);
                published.valid = true;
            }
            let recent = self.purchases_dirty.then(|| self.purchases.clone());
            if let Ok(mut s) = self.shared.lock() {
                s.snapshot = published;
                s.twitch.connected = connected;
                if let Some(recent) = recent {
                    self.purchases_dirty = false;
                    s.twitch.recent = recent;
                }
            }
        }

        // Никнеймы считаем ДО проверок видимости панели: подписи рисуются
        // поверх кадра и от видимости самой панели не зависят.
        self.enemy_tags.clear();
        // `has_owners` в условии: заспавненный за баллы носит ник своего
        // покупателя, и он известен даже когда чат не подключён вовсе - иначе
        // именно этот, оплаченный, враг остался бы без подписи.
        let name_someone = !self.viewers.is_empty() || self.spawn.has_owners();
        if self.config.enemy_tags && name_someone && !snapshot.menu_open && !snapshot.in_cutscene {
            let screen = ui.io().display_size;
            let owned = self.spawn.owned_handles();
            // Купленные подписываются и при снятой галочке «На врагах»: она
            // про обычных мобов, а за этих заплачено.
            let mut visible = enemies::native_tags(
                screen,
                [self.config.enemy_tag_offset_x, self.config.enemy_tag_height],
                &owned,
                self.config.enemy_tags_mobs,
            );
            // Боссы идут отдельным списком: над головой у них тега нет, а
            // полоску игра рисует внизу по центру. Строка на каждого, друг над
            // другом - при двойном боссе видно обоих.
            if self.config.enemy_tags_bosses {
                // Дедуп: купленного босса первый список уже мог взять своим
                // проходом, и без этого над ним висели бы две подписи.
                for tag in enemies::boss_tags(
                    screen,
                    [self.config.boss_tag_offset_x, self.config.boss_tag_offset_y],
                ) {
                    if !visible.iter().any(|t| t.handle == tag.handle) {
                        visible.push(tag);
                    }
                }
            }
            let handles: Vec<_> = visible.iter().map(|t| t.handle).collect();
            self.native_tags_written = visible.len();
            let names =
                self.nicknames.assign(&handles, &self.viewers, self.viewers_gen, std::time::Instant::now());
            // `None` - зрителей меньше, чем врагов в кадре: такой враг идёт
            // без подписи, дубль одного ника на всех выглядел бы поломкой.
            //
            // Идём по `visible`, а не по `names`: у купленного врага имя своё,
            // и оно должно появиться, даже когда ротация не выдала ничего
            // (пустой список чата - `assign` возвращает пустой вектор).
            self.enemy_tags = visible
                .iter()
                .enumerate()
                .filter_map(|(i, t)| {
                    let owner = self.spawn.owner_of(&t.handle).map(str::to_string);
                    let name = owner.or_else(|| names.get(i).cloned().flatten())?;
                    // «Последнее слово»: что этот зритель написал в чат
                    // последним. Чужой текст на экране стрима - поэтому только
                    // по галочке, и чат его уже почистил и обрезал.
                    let said = self
                        .config
                        .enemy_tag_say
                        .then(|| twitch::chat::said(&name))
                        .flatten()
                        .and_then(|(text, age)| fade_out(age, self.config.enemy_say_secs).map(|f| (text, f)));
                    Some((name, said, t.screen))
                })
                .collect();
        }
        // Тянут доводку подписи босса: живого босса под настройку не найти,
        // поэтому образец идёт от центра экрана - показывает сам сдвиг, а не
        // место над конкретной головой (его считает проекция).
        if self.preview_boss {
            let screen = ui.io().display_size;
            let (k, _) = enemies::ui_scale(screen);
            self.enemy_tags.push((
                i18n::t("Boss").to_string(),
                None,
                [
                    screen[0] * 0.5 + self.config.boss_tag_offset_x * k,
                    screen[1] * 0.5 + self.config.boss_tag_offset_y * k,
                ],
            ));
        }
        // Тянут ползунок подписи над врагом: живого врага под настройку не
        // найти, поэтому показываем образец в центре экрана. Позиция считается
        // тем же путём, что у настоящего тега - из виртуальных 1920x1080.
        if self.preview_tag && self.enemy_tags.is_empty() {
            let (k, pad) = enemies::ui_scale(ui.io().display_size);
            self.enemy_tags.push((
                i18n::t("Viewer").to_string(),
                self.config
                    .enemy_tag_say
                    .then(|| (i18n::t("look at this boss").to_string(), 1.0)),
                [
                    pad[0] + (enemies::UI_WIDTH * 0.5 + self.config.enemy_tag_offset_x) * k,
                    pad[1] + (enemies::UI_HEIGHT * 0.5 + self.config.enemy_tag_height) * k,
                ],
            ));
        }

        // Зеркалим собственный HUD игры: открытое меню (инвентарь, карта,
        // пауза, отдых у благодати) убирает её полоски - значит и нашей панели
        // там не место, если это не выключено настройкой. Катсцены `hud_state`
        // не трогают, поэтому проверка отдельная. Обе падают закрыто.
        let suppressed = !self.settings_open
            && ((self.config.hide_in_menu && snapshot.menu_open)
                || (self.config.hide_in_cutscene && snapshot.in_cutscene));

        // Купленные клавиши жмём только в обычной игре. Настройки тоже
        // перекрывают: там ввод и так забирает наше окно.
        self.boss_fight_now = snapshot.boss_fight_active;
        self.in_hub_now = snapshot.in_hub;
        let was_active = self.gameplay_active;
        // `game_focused` - не придирка, а починка: `SendInput` бьёт по
        // активному окну ВСЕЙ системы, и пока стример сидит в браузере,
        // купленная зрителем клавиша печатается туда.
        self.gameplay_active = snapshot.valid
            && !snapshot.menu_open
            && !snapshot.in_cutscene
            && !self.settings_open
            && input::game_focused();
        // Ушли из игры с зажатой клавишей - отпускаем немедленно, иначе она
        // останется нажатой во всей системе, а не только в игре.
        if was_active && !self.gameplay_active {
            self.actions.release_all();
            // Ушли из игры - неисполненное вернуть зрителям, а не выбросить.
            for left in self.actions.drain_queue() {
                self.refund(&left.reward_id, &left.redemption_id);
            }
            for (rid, red) in self.spawn.abandon_all() {
                self.refund(&rid, &red);
            }
        }
        // Игрок умер - отложенные покупки досчитывать некому: враг вылезет над
        // трупом или у благодати, то есть не там и не тогда, за что платили.
        // Уже стоящих в мире это не трогает, у них свой TTL.
        if snapshot.player_dead {
            for (rid, red) in self.spawn.abandon_delayed() {
                self.refund(&rid, &red);
            }
            // Смерть снимает эффекты (прямой запрос 2026-08-24) - кроме тех, что
            // её переживают, см. `effects::survives_death`. По фронту, а не
            // каждый кадр смерти: карточки снимать повторно незачем.
            if !self.was_dead {
                let cancelled = self.effects.revert_on_death();
                self.drop_purchases(&cancelled);
                if self.viewer_to_blame() {
                    self.collector.add_viewer_death();
                }
            }
        }
        self.was_dead = snapshot.player_dead;
        // Очередь двигается только в игре: в меню нажатия всё равно запрещены.
        if self.gameplay_active {
            if let Some(pending) = self.actions.advance(std::time::Instant::now()) {
                if meddles_with_play(&pending.action) {
                    self.viewer_meddled_at = Some(std::time::Instant::now());
                }
                match pending.action {
                    twitch::rewards::Action::SpawnEnemy { .. } => self.try_spawn(
                        pending.action,
                        pending.reward_id,
                        pending.redemption_id,
                        pending.viewer,
                    ),
                    twitch::rewards::Action::Effect { .. } => {
                        self.try_effect(pending.action, pending.reward_id, pending.redemption_id)
                    }
                    // Клавиша нажата прямо сейчас - покупка состоялась.
                    // Спавн метим не здесь, а когда враг реально появится:
                    // заявка ещё может протухнуть, а помеченное выполненным
                    // Twitch отменять уже не даёт.
                    _ => self.fulfill(&pending.reward_id, &pending.redemption_id),
                }
            }
            // TTL/результат заявки - только если реально есть что тикать: не
            // резолвим синглтон впустую, когда спавнов нет вовсе.
            if self.spawn.has_work() {
                let (radius, limit) = (self.config.spawn_radius_m, self.config.spawn_limit);
                let (refunds, done) = self.spawn.tick_world(
                    radius,
                    self.config.spawn_in_front,
                    limit,
                    self.config.spawn_same_limit,
                    std::time::Instant::now(),
                );
                for (rid, red) in refunds {
                    self.refund(&rid, &red);
                }
                for (rid, red) in done {
                    self.fulfill(&rid, &red);
                }
                // Чем кончилась заявка - в ту же строку окна, где причины
                // отказа. Единственный канал: лога в моде нет.
                if let Some(note) = self.spawn.take_note() {
                    self.reward_notice = Some(note);
                }
            }
        }
        // Отложенная проверка ждёт, пока игра снова примет ввод.
        if self.gameplay_active {
            if let Some((action, id)) = self.pending_test.take() {
                // Проверка открывает окно вины наравне с покупкой: иначе
                // счётчик «Смерти от зрителей» нечем проверить вообще.
                if meddles_with_play(&action) {
                    self.viewer_meddled_at = Some(std::time::Instant::now());
                }
                match action {
                    twitch::rewards::Action::Hold { key, duration_ms } => {
                        self.actions.hold(key, Duration::from_millis(duration_ms as u64));
                    }
                    twitch::rewards::Action::Press { key } => self.actions.press(key),
                    twitch::rewards::Action::SpawnEnemy { .. } => {
                        // Проверка из настроек: покупателя нет, подпись врагу
                        // достанется из обычной ротации зрителей. Id - тот же,
                        // что у уже показанной карточки, иначе «Снять
                        // эффекты»/уход из геймплея её не найдут.
                        self.try_spawn(action, String::new(), id, String::new());
                    }
                    twitch::rewards::Action::Effect { .. } => {
                        self.try_effect(action, String::new(), id);
                    }
                }
            }
        }
        self.actions.tick();
        // Безусловно, как и `actions.tick`: временный эффект живёт по стенным
        // часам, и уход в меню его не отменяет - зато без тика игра осталась
        // бы замедленной. Пустой список выходит первой же строкой, синглтоны
        // при этом не резолвятся.
        //
        // Счёт урона (рунический обмен) выключаем в меню и катсцене - живьём
        // отдых у костра начислял десятки тысяч рун разом: рестарт области
        // массово убирает врагов, и правило «удар наш, пока не доказано
        // обратное» списывало это на игрока (жалоба 2026-08-24).
        self.effects.tick(
            std::time::Instant::now(),
            !snapshot.menu_open && !snapshot.in_cutscene,
        );
        // Какой статус выпал и прочее - в ту же строку окна, где причины
        // отказа спавна. Единственный канал: лога в моде нет.
        if let Some(note) = self.effects.take_note() {
            self.reward_notice = Some(note);
        }

        // Диагностическая панель - стоковые виджеты и никаких проверок: если
        // видна она, а обычной панели нет, дело в наших данных или в ручной
        // отрисовке, а не в загрузке мода.
        if self.config.debug {
            overlay::draw_debug(ui, &snapshot, self.frames, suppressed, msg::probe());
        }

        // Окно настроек рисуется до всех проверок видимости и держит панель
        // на экране: правишь её вид - должен его видеть, даже если открыл
        // инвентарь.
        // Игра прячет системный курсор, поэтому пока окно открыто ImGui рисует
        // свой. Ставится каждый кадр по факту, а не по событиям открытия и
        // закрытия: закрыть окно можно тремя способами (F7, крестик, сброс), и
        // курсор оставался висеть на экране, если сбросить его забыли хоть в
        // одном из них.
        // Список боссов тоже нуждается в курсоре: по нему прокручивают и
        // ищут. Ставится по состоянию, а не по событию, - см. ниже.
        let boss_list = settings::boss_list_open();
        unsafe {
            (*hudhook::imgui::sys::igGetIO()).MouseDrawCursor = self.settings_open || boss_list
        };

        // Проявление и затухание окна настроек. Вниз быстрее, чем вверх - то
        // же правило и те же ощущения, что у панели ниже.
        const SETTINGS_FADE_IN: f32 = 0.15;
        const SETTINGS_FADE_OUT: f32 = 0.12;
        let sdt = ui.io().delta_time.clamp(0.0, 0.1);
        let starget = if self.settings_open { 1.0 } else { 0.0 };
        let sstep = sdt / if self.settings_fade < starget { SETTINGS_FADE_IN } else { SETTINGS_FADE_OUT };
        self.settings_fade = if self.settings_fade < starget {
            (self.settings_fade + sstep).min(starget)
        } else {
            (self.settings_fade - sstep).max(starget)
        };

        // Рисуем и после закрытия, пока идёт затухание. `open` при этом всегда
        // true: с false ImGui не нарисовал бы окно вовсе, и вместо плавного
        // ухода вышел бы щелчок.
        // Список боссов живёт отдельно от настроек: рисовать его только внутри
        // `settings::draw` значило бы, что F8 работает лишь при открытом F7 -
        // ровно на это и пожаловались (2026-08-27).
        settings::boss_list_window(ui, &self.config);
        if boss_list {
            input::hold_capture();
        }

        if self.settings_open || self.settings_fade > 0.01 {
            if self.settings_open {
                input::hold_capture();
            }
            let mut open = true;
            // Клон, а не удержание мьютекса на всё время отрисовки: сетевой
            // поток не должен ждать, пока нарисуется окно.
            let status = self.twitch_status.lock().map(|s| s.clone()).unwrap_or_default();
            let mut rewards = std::mem::take(&mut self.rewards);
            let out = settings::draw(
                ui,
                &mut self.config,
                &mut open,
                &mut self.panel_drag,
                &status,
                self.purchase_log.make_contiguous(),
                &mut rewards,
                &self.viewers,
                &mut self.capture_key_for,
                self.reward_notice.as_deref(),
                self.spawn.active_count(),
                self.effects.count(),
                self.settings_fade,
            );
            self.rewards = rewards;

            self.preview_toast = out.preview_toast;
            self.preview_fight = out.preview_fight;
            self.preview_tag = out.preview_tag;
            self.preview_boss = out.preview_boss;

            if out.refresh_viewers {
                self.refresh_viewers();
            }
            if let Some(on) = out.enable_all_rewards {
                self.enable_all_rewards(on);
            }
            // ЧС правится прямо в окне: выкидываем попавших в него сразу, не
            // дожидаясь следующего списка от чата или Helix.
            if out.changes.iter().any(|(k, _)| *k == "viewer_block") {
                self.drop_blocked();
                self.viewers_gen = self.viewers_gen.wrapping_add(1);
            }

            if out.clear_effects {
                let cancelled = self.effects.revert_all();
                let n = cancelled.len();
                self.drop_purchases(&cancelled);
                self.reward_notice =
                    Some(format!("{}: {n}", i18n::t("effects cleared")));
            }

            if out.audit_spawns {
                self.reward_notice = Some(spawn::audit_table());
            }

            if out.clear_spawns {
                let n = self.spawn.clear_all();
                self.reward_notice =
                    Some(format!("{}: {n}", i18n::t("spawned enemies removed")));
            }

            if out.forget_token {
                // Через флаг, а не прямым удалением файла: токен уже прочитан
                // сетевым потоком и живёт у него в памяти. Стереть файл значило
                // бы ничего не изменить до перезапуска игры - ровно на это и
                // была жалоба.
                twitch::request_forget();
                twitch::auth::forget_token(self.hmodule);
            }
            // Ждём нажатия: любая клавиша из тех, что мод умеет нажимать,
            // становится назначенной. Esc отменяет - иначе из режима захвата
            // было бы не выйти, не назначив что-нибудь случайное.
            if let Some(id) = self.capture_key_for {
                if ui.is_key_pressed(hudhook::imgui::Key::Escape) {
                    self.capture_key_for = None;
                } else if let Some(key) = twitch::rewards::KEYS
                    .iter()
                    // Свои клавиши назначать нельзя: купленная награда открыла
                    // бы окно настроек или перечитала .ini вместо действия в
                    // игре, а сам захват закрылся бы на том же нажатии.
                    .filter(|(k, _)| *k != self.config.settings_key)
                    // `no_repeat` и сверка с настоящей клавиатурой - обе из-за
                    // альт-таба: hudhook не получает событий потери фокуса, и
                    // Alt (или Win), которым увели окно, остаётся для ImGui
                    // нажатым навсегда. С автоповтором он назначался снова и
                    // снова (жалоба 2026-08-22), а `GetAsyncKeyState` про такую
                    // залипшую клавишу знает правду.
                    .find(|(k, vk)| ui.is_key_pressed_no_repeat(*k) && input::physically_down(*vk))
                    .map(|(k, _)| *k)
                {
                    if let Some(e) = self.rewards.iter_mut().find(|e| e.id == id) {
                        e.action = match e.action {
                            twitch::rewards::Action::Hold { duration_ms, .. } => {
                                twitch::rewards::Action::Hold { key, duration_ms }
                            }
                            twitch::rewards::Action::Press { .. } => twitch::rewards::Action::Press { key },
                            // Не достижимо: кнопка «Клавиша: ...» не рисуется
                            // для spawn-наград (см. settings::tab_rewards), но
                            // матч обязан быть исчерпывающим.
                            twitch::rewards::Action::SpawnEnemy { .. }
                            | twitch::rewards::Action::Effect { .. } => e.action,
                        };
                        twitch::rewards::save(self.hmodule, &self.rewards);
                    }
                    self.capture_key_for = None;
                }
            }

            if out.rewards_dirty {
                twitch::rewards::save(self.hmodule, &self.rewards);
            }
            if let Some(id) = out.create_reward {
                if let Some(e) = self.rewards.iter().find(|e| e.id == id) {
                    // Награда уже заведена - значит её надо не создать заново
                    // (Twitch не даст, названия уникальны), а привести к тому,
                    // что сейчас настроено в моде.
                    let (command, notice) = if e.reward_id.is_empty() {
                        (
                            twitch::Command::CreateReward {
                                local_id: id,
                                title: e.reward_title.trim().to_string(),
                                cost: e.cost,
                                mark: e.mark(),
                            },
                            i18n::t("creating the reward on Twitch..."),
                        )
                    } else {
                        (
                            twitch::Command::UpdateReward {
                                local_id: id,
                                reward_id: e.reward_id.clone(),
                                title: e.reward_title.trim().to_string(),
                                cost: e.cost,
                                mark: e.mark(),
                            },
                            i18n::t("updating the reward on Twitch..."),
                        )
                    };
                    if let Ok(mut q) = self.twitch_commands.lock() {
                        q.push_back(command);
                    }
                    self.reward_notice = Some(notice.to_string());
                }
            }
            if let Some(reward_id) = out.delete_on_twitch {
                if let Ok(mut q) = self.twitch_commands.lock() {
                    q.push_back(twitch::Command::DeleteReward { reward_id });
                }
            }
            if let Some(id) = out.test_reward {
                // Мимо сопоставления с Twitch: проверяют обычно ещё не
                // заполненную строку, а клавишу нажать надо всё равно.
                if let Some(e) = self.rewards.iter().find(|e| e.id == id).cloned() {
                    let caption = e.caption().to_string();
                    // То же время жизни, что и у настоящей покупки: иначе
                    // карточка проверяемого эффекта пропадала через пару секунд,
                    // пока сам эффект ещё шёл (жалоба 2026-08-24).
                    let (life, countdown_to) =
                        card_timing(Some(e.action), self.config.twitch_notify_secs);
                    // Свой id, не пустая строка: пустой `redemption_id`
                    // фильтруется из `revert()`/`abandon_all()`, и «Снять
                    // эффекты» или уход из геймплея не находили карточку
                    // проверки вообще (жалоба 2026-08-24). `reward_id`
                    // остаётся пустым - им `refund`/`fulfill` и так отсекают
                    // тестовые покупки от сети.
                    self.test_purchases += 1;
                    let test_id = format!("test#{}", self.test_purchases);
                    self.push_purchase_at(
                        i18n::t("Test").to_string(),
                        caption,
                        e.cost,
                        countdown_to,
                        life,
                        test_id.clone(),
                    );
                    self.pending_test = Some((e.action, test_id));
                    // Окно закрываем сами: пока оно открыто, ввод забирает наш
                    // детур, и проверка молча не дошла бы до игры.
                    open = false;
                }
            }

            if out.test_purchase {
                // Номер, чтобы подряд нажатые тесты отличались друг от друга -
                // иначе непонятно, появилась новая карточка или висит старая.
                self.test_purchases += 1;
                let viewer = i18n::t("Test").to_string();
                let label = format!("{} #{}", i18n::t("Display check"), self.test_purchases);
                // И в журнал тоже: иначе «Последние покупки» нечем проверить,
                // не имея зрителей - ровно та кнопка, которой это и делают.
                self.log_purchase(&viewer, &label, 100);
                self.push_purchase(viewer, label, 100);
            }

            if out.reset {
                self.config = Config::default();
                // Язык - глобальный, и сброс обязан вернуть и его тоже:
                // иначе после «Сбросить всё» .ini говорит одно, а меню другое.
                i18n::apply(&self.config.lang);
                Config::save_values(self.hmodule, &self.config.all_values());
                self.font_reload_pending = true;
            } else if !out.changes.is_empty() {
                Config::save_values(self.hmodule, &out.changes);
                // Атлас пересобирается только если поменялись кегли - это
                // проверяет `font_signature` в `before_render`.
                self.font_reload_pending = true;
            }
            if out.reset || !out.changes.is_empty() {
                if let Ok(mut sh) = self.shared.lock() {
                    sh.config = self.config.clone();
                }
            }
            if !open && self.settings_open {
                self.settings_open = false;
                input::release_capture();
                // Иначе следующее открытие окна встречает нас в режиме
                // «жду нажатия» и назначает награде первую же клавишу.
                self.capture_key_for = None;
            }
        }

        // Затухание: вниз быстрее, чем вверх - на выходе из меню мягкое
        // проявление читается лучше, чем щелчок.
        // ponytail: времена зашиты; вынести в конфиг, если попросят.
        const FADE_OUT_SECS: f32 = 0.18;
        const FADE_IN_SECS: f32 = 0.45;
        let dt = ui.io().delta_time.clamp(0.0, 0.1);
        // Гаснем сразу, проявляемся с задержкой: на загрузках `menu_open` и
        // флаг катсцены пропадают на кадр-другой, и без этого панель начинала
        // проявляться в каждую такую щель (та же жалоба про мигание).
        self.showable_for = if suppressed { 0.0 } else { self.showable_for + dt };
        let target = if warmed && self.showable_for >= SHOW_DEBOUNCE_SECS { 1.0 } else { 0.0 };
        let step = dt / if self.hud_fade < target { FADE_IN_SECS } else { FADE_OUT_SECS };
        self.hud_fade = if self.hud_fade < target {
            (self.hud_fade + step).min(target)
        } else {
            (self.hud_fade - step).max(target)
        };
        overlay::set_alpha(self.hud_fade);

        if !self.config.overlay_enabled {
            return;
        }
        // Карточки покупок рисуются даже когда панель погашена (меню, катсцена,
        // игра ещё грузится): за покупку заплачено, и показать её надо. Своя
        // прозрачность у каждой - поэтому строго ПОСЛЕ `overlay::draw`, чтобы
        // общий `set_alpha` панели уже был отработан.
        let show_toasts = self.config.twitch_notify_hud && !self.purchases.is_empty();

        if self.preview_fight {
            // Панель показывается даже в меню и на загрузке: настраивают её
            // обычно оттуда, и ждать боя ради одного ползунка незачем.
            let mut demo = snapshot.clone();
            demo_fight(&mut demo);
            overlay::set_alpha(1.0);
            overlay::draw(ui, &demo, &self.config);
        } else if snapshot.valid && self.hud_fade > 0.0 {
            overlay::draw(ui, &snapshot, &self.config);
        }

        // Своя подпись - только если движок за неё не взялся.
        if !self.enemy_tags.is_empty() {
            overlay::draw_enemy_tags(ui, &self.enemy_tags, &self.config);
        }
        if show_toasts {
            overlay::draw_purchase_toasts(ui, &self.purchases, &self.config, self.config.twitch_notify_secs);
        } else if self.preview_toast {
            // Двигают ручку положения карточки: показываем настоящую карточку
            // на её месте, иначе позиция выставляется вслепую - по одной точке
            // в рамке. Своя, а не из истории: покупок может не быть вовсе.
            // С отсчётом: карточка спавна выше обычной на строку, и
            // выставлять её положение надо по самому высокому варианту.
            let sample = [twitch::PurchaseEvent {
                viewer: i18n::t("Viewer").to_string(),
                label: i18n::t("Channel point reward").to_string(),
                cost: 100,
                // Возраст фиксирован и мал: анимация появления уже отыграна, а
                // до затухания далеко даже при минимальном «сколько висит» -
                // иначе образец пропадал бы ровно на том ползунке, которым его
                // и настраивают.
                at: std::time::Instant::now() - std::time::Duration::from_millis(400),
                countdown_to: Some(std::time::Instant::now() + std::time::Duration::from_secs(3)),
                // Образец живёт по общему ползунку - его тут и настраивают.
                life: None,
                redemption_id: String::new(),
            }];
            overlay::draw_purchase_toasts(ui, &sample, &self.config, self.config.twitch_notify_secs);
        }
    }
}

/// Прозрачность реплики над врагом по её возрасту.
///
/// `None` - уже догорела, рисовать нечего. Гаснет последние `SAY_FADE_SECS`,
/// а не по щелчку: исчезающая строка читается как «сказано давно», а
/// пропавшая в один кадр - как сбой мода.
fn fade_out(age: f32, life_secs: f32) -> Option<f32> {
    const SAY_FADE_SECS: f32 = 0.8;
    let left = life_secs - age;
    (left > 0.0).then(|| (left / SAY_FADE_SECS).clamp(0.0, 1.0))
}

/// Образец боя для предпросмотра: имя босса, номер попытки и часы.
///
/// Нужен ровно затем, чтобы ползунки «Имя босса» и «Попытка и время» было
/// видно на что влияют: вне боя этих строк на панели нет вовсе.
fn demo_fight(s: &mut stats::Snapshot) {
    s.boss_name = Some(i18n::t("Margit, the Fell Omen").to_string());
    s.attempts = s.attempts.max(3);
    s.attempt_secs = s.attempt_secs.max(84.0);
    s.deaths_on_boss = s.deaths_on_boss.max(2);
}

/// Пауза перед установкой оверлея.
///
/// **Найдено живьём 2026-08-17.** `wait_for_system_init` дожидается всего лишь
/// глобального `hInstance` у `CSWindow` - а это происходит сразу после
/// инициализации CRT, за секунды до того, как игра создаст D3D12-устройство и
/// swapchain. Установка оверлея в этот момент идёт параллельно с инициализацией
/// графики самой игрой, и запуск вешается: кадры приходят раз в полторы
/// секунды, до главного меню дело не доходит. Наш собственный код при этом
/// занимает 2 мкс на кадр - то есть виновата именно гонка на старте.
///
/// Ручка, а не константа: момент готовности зависит от машины, диска и набора
/// модов, и подобрать его "правильно" из кода нельзя.
fn wait_for_game_ready(delay_ms: u32) {
    std::thread::sleep(Duration::from_millis(delay_ms as u64));
}

#[no_mangle]
unsafe extern "system" fn DllMain(
    hmodule: windows::Win32::Foundation::HINSTANCE,
    reason: u32,
    _reserved: *mut std::ffi::c_void,
) {
    // Явная выгрузка (`FreeLibrary` менеджером модов): `lpReserved` при ней
    // null, при завершении процесса - нет, и там уже ничего делать не нужно.
    // Тела детуров живут в этой DLL: оставить пролог пропатченным и уйти -
    // значит оставить в user32 прыжок в незамапленную память.
    if reason == windows::Win32::System::SystemServices::DLL_PROCESS_DETACH && _reserved.is_null() {
        input::uninstall();
        return;
    }
    if reason != windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH {
        return;
    }
    let hmodule_raw = hmodule.0 as usize;
    std::thread::spawn(move || {
        let _init = wait_for_system_init(&Program::current(), Duration::MAX);

        // Конфиг нужен до установки оверлея - в нём пауза запуска.
        let config = Config::load(hmodule_raw);
        // Выученные имена боссов: файл рядом с DLL, читается один раз.
        bosses::init(hmodule_raw);
        spawn::init(hmodule_raw);
        wait_for_game_ready(config.startup_delay_ms);

        let hmodule = windows::Win32::Foundation::HINSTANCE(hmodule_raw as _);
        let hud = StreamHud::new(hmodule_raw, config);
        match Hudhook::builder()
            .with::<ImguiDx12Hooks>(hud)
            .with_hmodule(hmodule)
            .build()
            .apply()
        {
            Ok(_) => {
                // Детуры ввода ставим строго ПОСЛЕ успешного `apply()`: до него
                // мод может выгрузиться (`eject`), а выгрузка с пропатченным
                // прологом - это прыжок в незамапленную память.
                input::install();
            }
            Err(_) => {
                // Уходим только потому, что ничего не пропатчили: детуров у нас
                // нет вовсе, так что выгрузиться безопасно.
                eject();
            }
        }
    });
}
