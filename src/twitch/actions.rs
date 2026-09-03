//! Исполнение купленного: нажатия и удержания клавиш.
//!
//! Живёт на игровом потоке (`render`), как и всё остальное в моде: сеть только
//! приносит событие, а трогает игру этот модуль.
//!
//! Нажимаем через `SendInput` - обычный системный вызов, не патч чужого кода.
//! Детуров ради этого не нужно (см. `input.rs`).

use std::time::{Duration, Instant};

use hudhook::imgui::Key;

use crate::input;
use super::rewards::vk_of;

/// Насколько долгим бывает одно нажатие «press». Меньше кадра игра может не
/// заметить, больше - уже удержание.
const PRESS_MS: u64 = 80;

/// Сколько очередь ждёт после заявки на спавн, прежде чем взяться за
/// следующее действие.
///
/// Не про красоту: у движка ОДИН флаг `spawn` и одна `init_data`, и вторая
/// заявка до того, как он разобрал первую, затёрла бы её (см. `spawn::request`,
/// там же отказ `Busy`). Обычный зазор в 120 мс - это 7 кадров, и на просадке
/// FPS их могло не хватить: покупка отбивалась бы возвратом баллов по вине
/// самого мода. Заодно три врага не вываливаются в один кадр.
const SPAWN_SETTLE_MS: u64 = 600;

/// Потолок удержания. Не вкусовщина, а страховка: пока клавиша «нажата», она
/// нажата во всей системе, и падение игры в этот момент оставило бы её
/// зажатой уже в Windows. Чем короче окно, тем меньше цена такого совпадения.
pub const MAX_HOLD_MS: u32 = 10_000;

/// Одна зажатая сейчас клавиша.
struct Held {
    key: Key,
    vk: u16,
    /// До какого момента держим. Продлевается, а не заменяется: если двое
    /// зрителей купили «держать W» внахлёст, отпустить надо по позднейшему.
    until: Instant,
    /// Нажали её мы. `false` - игрок держал её сам ещё до покупки, и трогать
    /// её нельзя ни нажатием, ни отпусканием (см. `hold`).
    ours: bool,
}

/// Что ждёт своей очереди. Вместе с действием несём и то, чем вернуть баллы,
/// если исполнить так и не удастся.
pub struct Pending {
    pub action: super::rewards::Action,
    pub reward_id: String,
    pub redemption_id: String,
    /// Кто купил. Нужен спавну: заспавненный враг носит ник своего покупателя
    /// (см. `spawn::SpawnState`), а не случайного зрителя из ротации.
    pub viewer: String,
}

#[derive(Default)]
pub struct ActionState {
    held: Vec<Held>,
    /// Очередь: одновременные покупки исполняются по одной, иначе «шаг вперёд»
    /// и «прыжок» от двух зрителей склеиваются в одно невнятное движение.
    queue: std::collections::VecDeque<Pending>,
    /// До какого момента занято текущим действием.
    busy_until: Option<Instant>,
}

impl ActionState {
    /// Зажать клавишу на заданное время. Повторный вызов по той же клавише
    /// продлевает удержание, а не начинает второе.
    pub fn hold(&mut self, key: Key, duration: Duration) {
        let Some(vk) = vk_of(key) else { return };
        let until = Instant::now() + duration.min(Duration::from_millis(MAX_HOLD_MS as u64));
        if let Some(h) = self.held.iter_mut().find(|h| h.key == key) {
            // Только вперёд: короткая покупка не должна обрывать длинную.
            h.until = h.until.max(until);
            return;
        }
        // Клавиша уже нажата игроком - значит игра и так её видит, а наше
        // отпускание оборвало бы его на середине: бежит вперёд, зритель купил
        // «W на 3 секунды», и через 3 секунды бег прекращается сам собой.
        // Такую клавишу проводим по списку, но не трогаем.
        let ours = !input::physically_down(vk);
        if ours {
            input::key_down(vk);
        }
        self.held.push(Held { key, vk, until, ours });
    }

    /// Короткое нажатие - то же удержание, только фиксированной длины: иначе
    /// пришлось бы вести два разных состояния ради одного и того же.
    pub fn press(&mut self, key: Key) {
        self.hold(key, Duration::from_millis(PRESS_MS));
    }

    /// Отпускает всё, чьё время вышло. Зовётся каждый кадр из `render`.
    pub fn tick(&mut self) {
        let now = Instant::now();
        self.held.retain(|h| {
            if h.until > now {
                return true;
            }
            if h.ours {
                input::key_up(h.vk);
            }
            false
        });
    }

    /// Отпустить всё немедленно. Нужно на любом переходе, после которого мы
    /// перестаём тикать: выключили интеграцию, открыли настройки, ушли в меню.
    /// Без этого клавиша осталась бы зажатой во всей системе.
    pub fn release_all(&mut self) {
        for h in self.held.drain(..) {
            if h.ours {
                input::key_up(h.vk);
            }
        }
    }

    /// Ставит покупку в очередь. `None` - взяли, `Some(pending)` - очередь
    /// полна, и это надо вернуть зрителю.
    pub fn enqueue(&mut self, pending: Pending, limit: usize) -> Option<Pending> {
        if self.queue.len() >= limit.max(1) {
            return Some(pending);
        }
        self.queue.push_back(pending);
        None
    }

    /// Достаёт следующее действие, если предыдущее закончилось. Вызывается
    /// каждый кадр вместе с `tick`.
    ///
    /// Возвращённое `Pending` игровому потоку не нужно - карточка показывается
    /// в момент покупки, а не исполнения. Это наблюдаемость для тестов:
    /// «пошло/не пошло» иначе видно только по нажатой клавише в Windows.
    pub fn advance(&mut self, now: Instant) -> Option<Pending> {
        if self.busy_until.is_some_and(|until| until > now) {
            return None;
        }
        let next = self.queue.pop_front()?;
        let duration = match next.action {
            super::rewards::Action::Hold { duration_ms, .. } => {
                Duration::from_millis(duration_ms as u64).min(Duration::from_millis(MAX_HOLD_MS as u64))
            }
            super::rewards::Action::Press { .. } => Duration::from_millis(PRESS_MS),
            // Спавн не про клавиатуру: длительности у него нет, но движку
            // нужно время подхватить заявку - см. `SPAWN_SETTLE_MS`.
            super::rewards::Action::SpawnEnemy { .. } => Duration::from_millis(SPAWN_SETTLE_MS),
            // Эффект - одна запись в память игры, ждать после него нечего:
            // очередь двинется дальше через обычный зазор в 120 мс.
            super::rewards::Action::Effect { .. } => Duration::ZERO,
        };
        match next.action {
            super::rewards::Action::Hold { key, .. } => self.hold(key, duration),
            super::rewards::Action::Press { key } => self.press(key),
            // Ничего не делаем: SpawnEnemy не трогает held/SendInput. Реальную
            // запись в память игры делает lib.rs, получив отсюда `Some(next)`
            // - этот модуль по-прежнему ничего не знает про eldenring.
            // Ничего не делаем по той же причине, что и у спавна: запись в
            // память игры делает `lib.rs`, получив отсюда `Some(next)`.
            super::rewards::Action::SpawnEnemy { .. } | super::rewards::Action::Effect { .. } => {}
        }
        // Небольшой зазор между действиями: иначе два «нажать Space» подряд
        // игра видит как одно удержание.
        self.busy_until = Some(now + duration + Duration::from_millis(120));
        Some(next)
    }

    /// Погашения, которые ещё ждут своей очереди.
    ///
    /// Нужны разбору хвоста: у Twitch они числятся «не выполнено» наравне с
    /// проспанными, и без этого списка их вернули бы зрителю прямо перед тем,
    /// как исполнить.
    pub fn pending_ids(&self) -> impl Iterator<Item = &str> {
        self.queue.iter().map(|p| p.redemption_id.as_str())
    }

    /// Всё, что не успело исполниться. Нужно, когда игра уходит в меню или мод
    /// выключают: баллы за это придётся вернуть.
    pub fn drain_queue(&mut self) -> Vec<Pending> {
        self.busy_until = None;
        self.queue.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Удержание не длиннее потолка, даже если в файле написали час.
    #[test]
    fn hold_is_capped() {
        let mut s = ActionState::default();
        let before = Instant::now();
        s.hold(Key::W, Duration::from_secs(3600));
        let until = s.held[0].until;
        assert!(until <= before + Duration::from_millis(MAX_HOLD_MS as u64) + Duration::from_millis(50));
    }

    /// Две покупки внахлёст: короткая не должна обрывать длинную, и вторая
    /// клавиша не заводится второй записью.
    #[test]
    fn overlapping_holds_extend_not_restart() {
        let mut s = ActionState::default();
        s.hold(Key::W, Duration::from_millis(5000));
        let long = s.held[0].until;
        s.hold(Key::W, Duration::from_millis(10));
        assert_eq!(s.held.len(), 1, "одна клавиша - одна запись");
        assert_eq!(s.held[0].until, long, "короткая покупка не укорачивает удержание");

        s.hold(Key::W, Duration::from_millis(8000));
        assert!(s.held[0].until > long, "более длинная - продлевает");
    }

    #[test]
    fn tick_releases_expired_and_keeps_the_rest() {
        let mut s = ActionState::default();
        s.hold(Key::W, Duration::from_millis(0));
        s.hold(Key::A, Duration::from_secs(5));
        assert!(!s.held.is_empty());
        s.tick();
        assert_eq!(s.held.len(), 1, "истёкшая отпущена");
        assert_eq!(s.held[0].key, Key::A);
    }

    #[test]
    fn release_all_clears_everything() {
        let mut s = ActionState::default();
        s.hold(Key::W, Duration::from_secs(5));
        s.press(Key::Space);
        assert!(!s.held.is_empty());
        s.release_all();
        assert!(s.held.is_empty(), "release_all обязан отпустить всё");
    }

    fn pending(action: super::super::rewards::Action) -> Pending {
        Pending { action, reward_id: "rw".into(), redemption_id: "rd".into(), viewer: "зритель".into() }
    }

    /// Одновременные покупки исполняются по одной, а не сливаются в кашу.
    #[test]
    fn queue_runs_one_at_a_time() {
        use super::super::rewards::Action;
        let mut s = ActionState::default();
        let now = Instant::now();
        assert!(s.enqueue(pending(Action::Press { key: Key::Space }), 5).is_none());
        assert!(s.enqueue(pending(Action::Press { key: Key::W }), 5).is_none());
        assert_eq!(s.queue.len(), 2);

        assert!(s.advance(now).is_some(), "первое пошло сразу");
        assert_eq!(s.queue.len(), 1);
        assert!(s.advance(now).is_none(), "второе ждёт, пока занято");

        // Когда первое отработало, очередь двигается дальше.
        let later = now + Duration::from_secs(1);
        assert!(s.advance(later).is_some());
        assert_eq!(s.queue.len(), 0);
    }

    /// Очередь не бесконечная: сверх лимита покупку возвращаем, а не копим.
    #[test]
    fn queue_has_a_limit() {
        use super::super::rewards::Action;
        let mut s = ActionState::default();
        for _ in 0..3 {
            assert!(s.enqueue(pending(Action::Press { key: Key::Space }), 3).is_none());
        }
        let rejected = s.enqueue(pending(Action::Press { key: Key::Space }), 3);
        assert!(rejected.is_some(), "четвёртая при лимите 3 отклонена");
        assert_eq!(rejected.unwrap().redemption_id, "rd");
    }

    /// Уход в меню обязан отдать неисполненное наружу - за него надо вернуть
    /// баллы, а не молча выбросить.
    #[test]
    fn draining_returns_unspent_purchases() {
        use super::super::rewards::Action;
        let mut s = ActionState::default();
        s.enqueue(pending(Action::Press { key: Key::Space }), 5);
        s.enqueue(pending(Action::Press { key: Key::W }), 5);
        let left = s.drain_queue();
        assert_eq!(left.len(), 2);
        assert_eq!(s.queue.len(), 0);
    }

    /// Клавишу, которую игрок держит сам, мод не отпускает: иначе купленное
    /// «держать W» обрывало бы его собственный бег в момент своего конца.
    #[test]
    fn a_key_the_player_holds_is_left_alone() {
        let mut s = ActionState::default();
        s.hold(Key::W, Duration::from_millis(0));
        // Физическое состояние на тестовой машине не подделать, поэтому
        // проверяем сам механизм: чужую запись `tick` не трогает.
        s.held[0].ours = false;
        s.tick();
        assert!(s.held.is_empty(), "запись всё равно снимается по времени");

        let mut s = ActionState::default();
        s.hold(Key::A, Duration::from_secs(5));
        s.held[0].ours = false;
        s.release_all();
        assert!(s.held.is_empty());
    }

    /// Спавн - не клавиатура: очередь обязана его отработать (сдвинуть
    /// `busy_until`, как press/hold), но не трогать `held`/`SendInput`.
    #[test]
    fn advance_does_not_touch_held_for_spawn_enemy() {
        use super::super::rewards::Action;
        let mut s = ActionState::default();
        let now = Instant::now();
        s.enqueue(pending(Action::SpawnEnemy { key: "x", delay_secs: 0, cooldown_secs: 0 }), 5);
        assert!(s.advance(now).is_some(), "заявка на спавн обязана уйти из очереди");
        assert!(s.held.is_empty(), "спавн не должен нажимать клавиши");
        assert!(s.advance(now).is_none(), "второе действие ждёт своего зазора");
    }

    /// Клавиша без кода не должна попадать в список: иначе `tick` будет вечно
    /// «отпускать» то, что никогда не нажималось.
    ///
    /// `Escape` исключён из таблицы намеренно (им отменяется захват клавиши),
    /// геймпад - потому что это не клавиатура вовсе. В файл наград руками
    /// можно вписать и то, и другое.
    #[test]
    fn unknown_key_is_ignored() {
        for key in [Key::Escape, Key::GamepadStart, Key::MouseLeft] {
            let mut s = ActionState::default();
            s.hold(key, Duration::from_secs(1));
            assert!(s.held.is_empty(), "{key:?} без vk-кода не должна попадать в список");
        }
    }
}
