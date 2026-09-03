//! Никнеймы зрителей над обычными врагами.
//!
//! Только чтение игровой памяти плюс своя отрисовка поверх кадра - детуров тут
//! нет. Родной игровой виджет тегов для этого не годится: у него 8 слотов, и
//! заполняет их движок только под текущий лок-он, а подписать надо всех, кто
//! попал на экран.
//!
//! Боссов подписываем (`enemy_tags_bosses`, по умолчанию включено): их имя игра
//! рисует внизу экрана, в полоске босса, а над головой пусто - накладываться
//! нечему. У обычных именных врагов наоборот, поэтому их мы пропускаем.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use eldenring::position::HavokPosition;

use eldenring::cs::{CSCamera, CSFeManImp, FieldInsHandle, NpcParam, SoloParamRepository, WorldChrMan};
use fromsoftware_shared::FromStatic;

/// Сколько держим никнейм за врагом, которого перестали видеть. Отошёл за угол
/// и вернулся - подпись та же, а не новая: мелькание имён читается как баг.
const FORGET_AFTER: Duration = Duration::from_secs(30);

/// Как часто пробуем построить множество именных врагов, пока парамы не
/// загружены. Один проход - это тысячи строк парама.
const NAMED_RETRY: Duration = Duration::from_millis(500);

/// Строки `NpcParam`, у которых есть собственное имя.
///
/// Игра рисует такому врагу его имя сама (те самые «красные» вроде Outrider
/// Knight), и наша подпись легла бы поверх - на скриншоте это выглядело кашей.
///
/// Строится один раз: парамы в рантайме не меняются, а искать строку в параме
/// на каждый тег каждый кадр значило бы линейный проход по тысячам строк.
fn named_rows() -> Option<&'static HashSet<u32>> {
    static NAMED: OnceLock<HashSet<u32>> = OnceLock::new();
    if let Some(set) = NAMED.get() {
        return Some(set);
    }
    // Дросселируем ДО `instance()` - то же правило, что у `poll_bosses` в
    // stats.rs: пока парамы не загружены, КАЖДАЯ попытка стоит полного прохода
    // по тысячам строк, а результат всё равно пуст.
    static LAST_TRY: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST_TRY.lock().ok()?;
        if last.is_some_and(|t| t.elapsed() < NAMED_RETRY) {
            return None;
        }
        *last = Some(Instant::now());
    }
    let repo = unsafe { SoloParamRepository::instance() }.ok()?;
    let set: HashSet<u32> =
        repo.rows::<NpcParam>().filter(|(_, r)| r.name_id() > 0).map(|(id, _)| id).collect();
    // До загрузки парамов итератор пуст - пустое множество не запоминаем,
    // попробуем на следующем кадре (тот же приём, что у реестра боссов).
    if set.is_empty() {
        return None;
    }
    Some(NAMED.get_or_init(|| set))
}

/// Есть ли у этого врага собственное имя, которое игра нарисует сама.
///
/// Множество и `WorldChrMan` приходят параметрами, а не резолвятся внутри:
/// тегов до восьми, и поиск синглтона на каждый - это восемь поисков за кадр
/// вместо одного (то же правило, что и в `collect`).
fn has_own_name(world: &WorldChrMan, named: &HashSet<u32>, handle: &FieldInsHandle) -> bool {
    world
        .chr_ins_by_handle(handle)
        .and_then(|c| u32::try_from(c.npc_param_id).ok())
        .is_some_and(|id| named.contains(&id))
}

/// Хэндлы тех, у кого сейчас полоска босса.
fn boss_handles() -> Vec<FieldInsHandle> {
    let Ok(fe) = (unsafe { CSFeManImp::instance() }) else {
        return Vec::new();
    };
    fe.boss_health_displays
        .iter()
        .map(|d| d.field_ins_handle)
        .filter(|h| !h.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Родной виджет игры
// ---------------------------------------------------------------------------
//
// У игры мы берём только ПОЗИЦИЮ тега и отбор («кого игра сейчас показывает»).
// Сам текст рисует мод своей отрисовкой.
//
// Заставить движок нарисовать ник самому - проверено живьём и отвергнуто:
// работает (через `NpcParam::name_id` и строку FMG), но цвет остаётся
// красным, фирменным цветом имён врагов. Его выставляет сам gfx-клип игры,
// уже после установки строки: `role_name_color` не действует, `<font color>`
// игнорируется, при том что `<font size>` работает - то есть разметка
// применяется, и игнорируется ровно цвет. Из рантайма перебить нечем.

/// Тег врага, как его расположила сама игра.
///
/// Позиция берётся у движка, а не считается нами: она уже посчитана для
/// полоски HP, и подпись гарантированно стоит там же, где полоска, - без
/// собственной проекции и без её главного риска (знак осей камеры).
pub struct NativeTag {
    pub handle: FieldInsHandle,
    /// Куда ставить подпись, в пикселях экрана. Игра отдаёт эту точку в
    /// виртуальных 1920x1080 (выяснено живьём 2026-08-19), пересчёт делает
    /// `native_tags` - наружу отсюда выходят уже пиксели.
    pub screen: [f32; 2],
}

/// Виртуальное разрешение игрового интерфейса.
///
/// Координаты тега игра считает именно в нём, а не в пикселях экрана - на 2K
/// подпись уезжала ровно в этой пропорции (живьём 2026-08-19: игра дала
/// x 893 при экране 2560, наша проекция того же врага - 1203, а
/// 893 * 2560/1920 = 1191).
pub const UI_WIDTH: f32 = 1920.0;
pub const UI_HEIGHT: f32 = 1080.0;

/// Выше этой линии игра полоску врага не рисует - вжимает обратно в кадр,
/// чтобы та не налезла на компас и верхний HUD.
///
/// Наша подпись про этот предел не знала и оставалась там, куда спроецировался
/// сам враг: на скриншоте 2026-08-23 полоска стояла на `y 174`, а ник на
/// `y 35` - разъезд в 139 единиц при обычных 17.
///
/// Число снято с того самого скриншота, а не выведено из кода игры. Если
/// подпись у верхнего края всё ещё не садится на полоску - двигать надо его,
/// и `debug = true` показывает обе координаты рядом.
const BAR_TOP_LIMIT: f32 = 174.0;

/// То же самое по горизонтали: полоска у игры фиксированной ширины, и у
/// бокового края она не вылезает за кадр, а сдвигается внутрь.
///
/// Замер по скриншоту 2026-08-23 (враг у правого края): полоска шириной 141
/// единицу стояла левым краем на `x 1718`, то есть `1920 - 61 - 141`.
///
/// **`screen_pos.x` - это ЦЕНТР полоски, а не её левый край** (сопоставление
/// двух кропов 2026-08-24: у левого края ник стоял ровно на полоске, у правого
/// - на 70 единиц левее неё). Оттого и дефолтный `enemy_tag_offset_x = -70`:
/// он ровно `-BAR_W/2` и переводит центр в левый край. Пределы поэтому
/// считаются для ЦЕНТРА - иначе правый упор срабатывает на полширины раньше и
/// утаскивает подпись влево.
pub const BAR_W: f32 = 141.0;
pub const BAR_SIDE_MARGIN: f32 = 61.0;

/// Снизу игра держит тег так же, как с боков. Отдельного замера нет - о
/// нижнем краю не жаловались, - поэтому берём тот же отступ, что и сбоку.
const BAR_BOTTOM_MARGIN: f32 = 61.0;

/// Во что превращать виртуальные координаты интерфейса: масштаб и поля.
///
/// Масштаб РАВНОМЕРНЫЙ (`min` по осям), а остаток уходит в поля по краям -
/// так же, как игра кладёт свой интерфейс на нестандартный экран. Раньше x
/// делился на ширину, а y на высоту: на 16:9 это одно и то же число, а на
/// ultrawide (2560x1080) сдвиг по горизонтали уезжал бы в 1.33 раза дальше
/// нужного, потому что полоска HP от ширины экрана не растягивается.
///
/// Отсюда же берётся масштаб кегля: подпись обязана расти вместе с полоской.
pub fn ui_scale(screen: [f32; 2]) -> (f32, [f32; 2]) {
    let k = (screen[0] / UI_WIDTH).min(screen[1] / UI_HEIGHT);
    let pad = [(screen[0] - UI_WIDTH * k) * 0.5, (screen[1] - UI_HEIGHT * k) * 0.5];
    (k, pad)
}

/// Теги врагов, которые игра прямо сейчас показывает.
///
/// Здесь же и отбор: `is_visible` плюс то же условие по таймеру, по которому
/// игра сама убирает тег. Боссы исключены - у них своя полоска с именем.
/// `offset` - сдвиг подписи в ЕДИНИЦАХ ИНТЕРФЕЙСА (1920x1080), а не в пикселях
/// экрана: иначе одно и то же значение уводило бы подпись по-разному на 1080p
/// и на 2K. Применяется до пересчёта в пиксели, вместе с самой координатой.
/// `always` - те, кого подписываем несмотря ни на что: враги, заспавненные за
/// баллы канала. Над таким висит ник его покупателя, и потерять эту подпись
/// из-за общего правила («у босса имя своё», «именной рисует себя сам») значит
/// не показать оплаченное.
///
/// **Купленному мало обойти правила отбора - надо обойти сам его источник.**
/// Тег игра заводит только под наведение, лок-он и свежий урон; враг, которого
/// в `enemy_chr_tag_displays` нет, в итератор не попадал физически, и
/// `always` до него не доходил (найдено разбором 2026-09-03). Поэтому вторым
/// проходом такие берутся по мировой точке, своей проекцией - тем же путём,
/// что и боссы в `boss_tags`.
///
/// `mobs` - подписывать ли обычных врагов вовсе. При `false` остаётся только
/// второй проход: галочка «На врагах» купленных не касается.
pub fn native_tags(
    screen: [f32; 2],
    offset: [f32; 2],
    always: &[FieldInsHandle],
    mobs: bool,
) -> Vec<NativeTag> {
    let Ok(fe) = (unsafe { CSFeManImp::instance() }) else {
        return Vec::new();
    };
    // Сначала самая дешёвая проверка: показывает ли игра хоть один тег. Слотов
    // у неё восемь, и чаще всего все пустые - а всё, что ниже, стоит двух
    // резолвов синглтона и прохода по параму. Раньше это считалось каждый
    // кадр впустую, хотя подписывать было некого.
    let any_shown = mobs
        && fe
            .enemy_chr_tag_displays
            .iter()
            .any(|t| t.is_visible && t.last_update_time_delta <= 1.5 && !t.field_ins_handle.is_empty());
    if !any_shown && always.is_empty() {
        return Vec::new();
    }
    let bosses = boss_handles();
    // Из виртуального разрешения интерфейса в пиксели экрана.
    let (k, pad) = ui_scale(screen);
    // Оба резолва - один раз на кадр, до обхода тегов. Внутри фильтра они шли
    // бы по разу на КАЖДЫЙ тег.
    let named = named_rows();
    let world = unsafe { WorldChrMan::instance() }.ok();
    let drawn = drawn_positions(fe);

    let mut tags: Vec<NativeTag> = fe
        .enemy_chr_tag_displays
        .iter()
        .filter(|_| any_shown)
        .filter(|t| t.is_visible && t.last_update_time_delta <= 1.5)
        .filter(|t| !t.field_ins_handle.is_empty())
        .filter(|t| {
            // Купленный зрителем - всегда: над ним ник покупателя.
            if always.contains(&t.field_ins_handle) {
                return true;
            }
            // Босса здесь не подписываем никогда: у него своя полоска внизу
            // экрана, и строка ему полагается там же (`boss_tags`). Над
            // головой у босса тега обычно нет вовсе - оттого «показывать имена
            // над боссами» и не работало (жалоба 2026-08-21).
            //
            // У обычного именного врага имя ровно там же, где наша подпись, и
            // две надписи накладывались друг на друга - пропускаем.
            if bosses.contains(&t.field_ins_handle) {
                false
            } else {
                match (world.as_ref(), named) {
                    (Some(w), Some(n)) => !has_own_name(w, n, &t.field_ins_handle),
                    // Парамы или мир ещё не готовы - подписываем всех: пустая
                    // подпись хуже лишней, а через кадр всё встанет на место.
                    _ => true,
                }
            }
        })
        .map(|t| {
            let mut at = at_bar(&drawn, &t.field_ins_handle)
                .unwrap_or([t.screen_pos.0, t.screen_pos.1]);
            // Те же отступы от краёв, что и у самой игры - со всех четырёх
            // сторон. Уже поджатой координате они не мешают: `clamp` от
            // подходящего числа ничего не меняет.
            let half = BAR_W * 0.5;
            at[0] = at[0].clamp(BAR_SIDE_MARGIN + half, UI_WIDTH - BAR_SIDE_MARGIN - half);
            at[1] = at[1].clamp(BAR_TOP_LIMIT, UI_HEIGHT - BAR_BOTTOM_MARGIN);
            NativeTag {
                handle: t.field_ins_handle,
                screen: [
                    pad[0] + (at[0] + offset[0]) * k,
                    pad[1] + (at[1] + offset[1]) * k,
                ],
            }
        })
        .collect();

    // Купленные, которым игра тега не завела. Позицию берём у самого существа
    // и в поля полоски НЕ вжимаем: полоски у него нет, и прижимать подпись к
    // краю было бы враньём - врага там нет.
    //
    // Поэтому же точку вне кадра отбрасываем целиком. `world_to_screen` сам
    // отсекает только то, что позади камеры, а отрисовка прижимает подпись к
    // экрану - без этой проверки ник врага сбоку висел бы у края.
    for handle in always {
        if tags.iter().any(|t| t.handle == *handle) {
            continue;
        }
        let Some(w) = world.as_ref() else { break };
        let Some(chr) = w.chr_ins_by_handle(handle) else {
            continue;
        };
        // Труп не подписываем: ник покупателя держится за врагом ещё
        // `GHOST_LINGER` после смерти - ровно чтобы не мигать, пока игра
        // дорисовывает его полоску, - а своей проекцией мы нарисовали бы его
        // и над лежащим телом.
        if chr.modules.data.hp <= 0 {
            continue;
        }
        let p = chr.modules.physics.position;
        let head = chr.modules.physics.hit_height.max(0.5);
        let Some(at) = world_to_screen(HavokPosition(p.0, p.1 + head, p.2, p.3), screen) else {
            continue;
        };
        if at[0] < 0.0 || at[0] > screen[0] || at[1] < 0.0 || at[1] > screen[1] {
            continue;
        }
        tags.push(NativeTag {
            handle: *handle,
            screen: [at[0] + offset[0] * k, at[1] + offset[1] * k],
        });
    }
    tags
}

/// Где игра НА САМОМ ДЕЛЕ нарисует полоску, по хэндлу.
///
/// `enemy_chr_tag_displays` - сырая проекция врага, а к интерфейсу едет копия
/// в `frontend_values.enemy_chr_tag_data`, и у края экрана она отличается:
/// полоску игра вжимает обратно в кадр, а сырая точка остаётся снаружи. На
/// скриншоте 2026-08-23 это и видно - полоска съехала вниз, ник остался
/// наверху.
///
/// Единицы те же самые (виртуальные 1920x1080), поэтому пересчёт не меняется.
fn drawn_positions(fe: &CSFeManImp) -> Vec<(FieldInsHandle, [f32; 2])> {
    fe.frontend_values
        .enemy_chr_tag_data
        .iter()
        .filter(|t| t.is_visible && !t.field_ins_handle.is_empty())
        .map(|t| (t.field_ins_handle, [t.screen_pos_x as f32, t.screen_pos_y as f32]))
        .collect()
}

fn at_bar(drawn: &[(FieldInsHandle, [f32; 2])], handle: &FieldInsHandle) -> Option<[f32; 2]> {
    drawn.iter().find(|(h, _)| h == handle).map(|(_, p)| *p)
}

/// Мировая точка в пиксели экрана.
///
/// Нужен только боссам: у обычного врага позицию даёт сама игра
/// (`ChrEnemyTagEntry::screen_pos`), а босса в теги она не кладёт вовсе -
/// у него своя полоска внизу экрана.
///
/// `None` - точка позади камеры.
/// ponytail: знак `forward`/`up` живьём не проверялся; если подписи зеркалятся
/// или улетают за экран - смотреть сюда первым делом.
fn world_to_screen(pos: HavokPosition, screen: [f32; 2]) -> Option<[f32; 2]> {
    let cam = unsafe { CSCamera::instance() }.ok()?;
    let cam = &cam.pers_cam_1;
    let (right, up, forward, eye) = (cam.matrix.0, cam.matrix.1, cam.matrix.2, cam.matrix.3);

    let rel = (pos.0 - eye.0, pos.1 - eye.1, pos.2 - eye.2);
    let dot3 = |a: (f32, f32, f32), b: fromsoftware_shared::F32Vector4| a.0 * b.0 + a.1 * b.1 + a.2 * b.2;

    let cz = dot3(rel, forward);
    if cz <= 0.1 {
        return None;
    }
    let tan_half_fov = (cam.fov * 0.5).tan();
    if tan_half_fov.abs() < f32::EPSILON || cam.aspect_ratio.abs() < f32::EPSILON {
        return None;
    }
    let ndc_x = (dot3(rel, right) / cz) / (tan_half_fov * cam.aspect_ratio);
    let ndc_y = (dot3(rel, up) / cz) / tan_half_fov;
    Some([(ndc_x * 0.5 + 0.5) * screen[0], (1.0 - (ndc_y * 0.5 + 0.5)) * screen[1]])
}

/// Ники боссов - над головой, своей проекцией.
///
/// **Позиции у боссовой полоски в игре нет:** `BossHealthDisplayEntry` несёт
/// только fmg-id, хэндл и полученный урон, а в теги над головой игра боссов не
/// кладёт - оттого галочка «подписывать боссов» и не работала. Поэтому берём
/// мировую точку самого босса, поднимаем на высоту его же коллизионной капсулы
/// (`hit_height` - у мухи и у тролля она разная, угадывать нечего) и проецируем
/// камерой.
///
/// `offset` - доводка в единицах интерфейса (1920x1080), как у обычных подписей.
pub fn boss_tags(screen: [f32; 2], offset: [f32; 2]) -> Vec<NativeTag> {
    let Ok(fe) = (unsafe { CSFeManImp::instance() }) else {
        return Vec::new();
    };
    let Ok(world) = (unsafe { WorldChrMan::instance() }) else {
        return Vec::new();
    };
    let (k, _) = ui_scale(screen);
    // Дедуп по хэндлу: один и тот же босс в двух записях дал бы две подписи в
    // одной точке, и обе - с одним ником.
    let mut seen: Vec<FieldInsHandle> = Vec::new();
    fe.boss_health_displays
        .iter()
        .filter(|d| !d.field_ins_handle.is_empty())
        .filter(|d| {
            let fresh = !seen.contains(&d.field_ins_handle);
            if fresh {
                seen.push(d.field_ins_handle);
            }
            fresh
        })
        .filter_map(|d| {
            let chr = world.chr_ins_by_handle(&d.field_ins_handle)?;
            let p = chr.modules.physics.position;
            let head = chr.modules.physics.hit_height.max(0.5);
            let at = world_to_screen(HavokPosition(p.0, p.1 + head, p.2, p.3), screen)?;
            Some(NativeTag {
                handle: d.field_ins_handle,
                screen: [at[0] + offset[0] * k, at[1] + offset[1] * k],
            })
        })
        .collect()
}

/// Кто из зрителей закреплён за каким врагом.
///
/// Держится между кадрами: имя, прыгающее с врага на врага, читается как
/// мельтешение, а не как «этого зовут так».
#[derive(Default)]
pub struct NicknameAssigner {
    assigned: HashMap<FieldInsHandle, (String, Instant)>,
    /// С какого места в списке зрителей выдавать следующее имя.
    cursor: usize,
    /// Поколение списка зрителей, на котором последний раз чистились выдачи.
    /// Список бывает длиной в тысячи (`chat::MAX_TRACKED`), а меняется раз в
    /// несколько секунд - сверять с ним каждый кадр незачем.
    seen_gen: u64,
    /// Когда ник последний раз показывали. Пока не остынет,он уступает место
    /// другим - иначе на маленьком чате один и тот же ник висит подряд.
    shown_at: HashMap<String, Instant>,
}

/// Сколько ник «отдыхает» после показа, уступая очередь другим.
const NAME_COOLDOWN: Duration = Duration::from_secs(90);

impl NicknameAssigner {
    /// Раздаёт имена видимым врагам. `None` - подписывать нечем.
    ///
    /// Имя в кадре не повторяется НИКОГДА: когда зрителей меньше, чем врагов,
    /// лишние остаются без подписи. Раньше они получали дубли, и на чате из
    /// одного человека все враги на экране назывались одинаково - выглядело
    /// это не как имена, а как ошибка (жалоба 2026-08-19).
    pub fn assign(
        &mut self,
        visible: &[FieldInsHandle],
        viewers: &[String],
        viewers_gen: u64,
        now: Instant,
    ) -> Vec<Option<String>> {
        if viewers.is_empty() {
            self.assigned.clear();
            return Vec::new();
        }

        // Забываем тех, кого давно не видели, - иначе карта пополняется до
        // конца сессии.
        self.assigned.retain(|_, (_, seen)| now.duration_since(*seen) < FORGET_AFTER);
        // Зритель ушёл из чата - его имя больше не выдаём. Только когда список
        // реально сменился: собрать множество из тысяч ников на КАЖДОМ кадре
        // дороже самой задачи, а новый список приезжает раз в несколько секунд.
        if self.seen_gen != viewers_gen {
            self.seen_gen = viewers_gen;
            let live: HashSet<&str> = viewers.iter().map(String::as_str).collect();
            self.assigned.retain(|_, (name, _)| live.contains(name.as_str()));
        }
        // Остывшие ники снова в общей очереди.
        self.shown_at.retain(|_, at| now.duration_since(*at) < NAME_COOLDOWN);

        let mut used: Vec<String> = Vec::with_capacity(visible.len());
        let mut result = Vec::with_capacity(visible.len());

        for handle in visible {
            // Уже носит имя - оставляем за ним. Но только если это имя не
            // занято другим врагом ПРЯМО СЕЙЧАС: враг мог пропасть из кадра,
            // его ник достаться соседу, а потом он вернуться - и над двойным
            // боссом оказывались два одинаковых ника (жалоба 2026-08-21).
            // Тогда старая привязка снимается, и он получает свободное имя.
            let taken = self.assigned.get(handle).is_some_and(|(n, _)| used.contains(n));
            if taken {
                self.assigned.remove(handle);
            } else if let Some((name, seen)) = self.assigned.get_mut(handle) {
                *seen = now;
                let name = name.clone();
                used.push(name.clone());
                result.push(Some(name));
                continue;
            }
            let Some(name) = self.take_free(viewers, &used, now) else {
                // Свободных имён не осталось - этот враг идёт без подписи.
                // В `assigned` не пишем: в следующем кадре, когда состав в
                // кадре сменится, ему может достаться имя.
                result.push(None);
                continue;
            };
            used.push(name.clone());
            self.assigned.insert(*handle, (name.clone(), now));
            result.push(Some(name));
        }
        result
    }

    /// Следующее имя по кругу: не занятое в этом кадре и по возможности не
    /// показанное недавно. `None` - все имена уже разобраны в этом кадре.
    ///
    /// Два прохода, а не один: сначала ищем полностью свободное имя, и только
    /// если все остыть не успели - берём просто незанятое. Иначе на чате из
    /// трёх человек подписывать было бы некем.
    fn take_free(&mut self, viewers: &[String], used: &[String], now: Instant) -> Option<String> {
        for fresh_only in [true, false] {
            for _ in 0..viewers.len() {
                // Клонируем только победителя: отвергнутый кандидат стоил
                // аллокации на каждом шаге, а шагов до 2 x числа зрителей.
                let name = &viewers[self.cursor % viewers.len()];
                self.cursor = (self.cursor + 1) % viewers.len();
                if used.iter().any(|u| u == name) {
                    continue;
                }
                if fresh_only && self.shown_at.contains_key(name) {
                    continue;
                }
                self.shown_at.insert(name.clone(), now);
                return Some(name.clone());
            }
        }
        // Зрителей меньше, чем врагов в кадре: дубль хуже пустоты - несколько
        // врагов с одним ником читаются как поломка, а не как имена.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eldenring::cs::{BlockId, FieldInsSelector, FieldInsType};

    fn handle(index: u32) -> FieldInsHandle {
        FieldInsHandle {
            selector: FieldInsSelector::from_parts(FieldInsType::Chr, 0, index),
            block_id: BlockId::from_parts(10, 1, 0, 0),
        }
    }

    fn viewers(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// Пока зрителей хватает, двое врагов не должны получить одно имя.
    #[test]
    fn names_do_not_repeat_while_viewers_last() {
        let mut a = NicknameAssigner::default();
        let now = Instant::now();
        let visible = [handle(1), handle(2), handle(3)];
        let names: Vec<String> =
            a.assign(&visible, &viewers(&["alpha", "beta", "gamma"]), 1, now).into_iter().flatten().collect();
        assert_eq!(names.len(), 3);
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "все три имени разные: {names:?}");
    }

    /// Врагов больше, чем зрителей: лишние остаются БЕЗ подписи, а не с
    /// копией того же ника. Три одинаковых имени на экране читаются как
    /// ошибка мода (жалоба 2026-08-19).
    #[test]
    fn a_name_is_never_shown_on_two_enemies_at_once() {
        // Враг пропал из кадра, его ник достался соседу, враг вернулся - над
        // двойным боссом оказывались два одинаковых ника.
        let mut a = NicknameAssigner::default();
        let viewers = vec!["alpha".to_string()];
        let now = Instant::now();
        let (one, two) = (handle(1), handle(2));

        assert_eq!(a.assign(&[one], &viewers, 1, now), vec![Some("alpha".into())]);
        // Первого не видно - имя уходит второму (зритель в чате всего один).
        assert_eq!(a.assign(&[two], &viewers, 1, now), vec![Some("alpha".into())]);
        // Оба в кадре: имя носит ровно один, второй остаётся без подписи.
        let both = a.assign(&[one, two], &viewers, 1, now);
        assert_eq!(both.iter().filter(|n| n.as_deref() == Some("alpha")).count(), 1, "{both:?}");
        assert!(both.contains(&None), "лишнему врагу подписи не достаётся: {both:?}");
    }

    #[test]
    fn extra_enemies_stay_unnamed_instead_of_duplicating() {
        let mut a = NicknameAssigner::default();
        let now = Instant::now();
        let visible = [handle(1), handle(2), handle(3)];
        let names = a.assign(&visible, &viewers(&["solo"]), 1, now);
        assert_eq!(names, vec![Some("solo".to_string()), None, None]);

        // Двое зрителей на трёх врагов - два имени, третий без подписи.
        let mut a = NicknameAssigner::default();
        let names = a.assign(&visible, &viewers(&["one", "two"]), 1, now);
        assert_eq!(names.iter().filter(|n| n.is_some()).count(), 2);
        assert_eq!(names[2], None);
    }

    /// Тот же враг в следующем кадре обязан сохранить своё имя.
    #[test]
    fn same_enemy_keeps_its_name() {
        let mut a = NicknameAssigner::default();
        let now = Instant::now();
        let people = viewers(&["alpha", "beta"]);
        let first = a.assign(&[handle(1), handle(2)], &people, 1, now);
        let second = a.assign(&[handle(1), handle(2)], &people, 1, now + Duration::from_secs(1));
        assert!(first.iter().all(Option::is_some), "двоим зрителям хватает на двоих врагов");
        assert_eq!(first, second, "имена не должны перескакивать между кадрами");

        // И даже если в кадре остался только один из них.
        let third = a.assign(&[handle(2)], &people, 1, now + Duration::from_secs(2));
        assert_eq!(third, vec![second[1].clone()]);
    }

    /// Ушедший из чата зритель не должен остаться висеть над врагом.
    #[test]
    fn viewer_who_left_is_dropped() {
        let mut a = NicknameAssigner::default();
        let now = Instant::now();
        let first = a.assign(&[handle(1)], &viewers(&["alpha"]), 1, now);
        assert_eq!(first, vec![Some("alpha".to_string())]);

        // Поколение другое: список чата сменился, и мод обязан это заметить.
        let second = a.assign(&[handle(1)], &viewers(&["beta"]), 2, now + Duration::from_secs(1));
        assert_eq!(second, vec![Some("beta".to_string())]);

        // А на том же поколении сверка не делается вовсе - это и есть экономия.
        let third = a.assign(&[handle(1)], &viewers(&["beta"]), 2, now + Duration::from_secs(2));
        assert_eq!(third, vec![Some("beta".to_string())]);
    }

    /// Свежих зрителей мод обязан предпочитать тем, кто уже висел над врагом:
    /// иначе на маленьком чате один и тот же ник показывается подряд.
    #[test]
    fn recently_shown_names_yield_to_others() {
        let mut a = NicknameAssigner::default();
        let now = Instant::now();
        let people = viewers(&["alpha", "beta", "gamma"]);

        let first = a.assign(&[handle(1)], &people, 1, now);
        let second = a.assign(&[handle(2)], &people, 1, now + Duration::from_secs(1));
        let third = a.assign(&[handle(3)], &people, 1, now + Duration::from_secs(2));
        let mut all: Vec<String> = first.into_iter().chain(second).chain(third).flatten().collect();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 3, "три разных врага - три разных зрителя");
    }

    /// Когда все ники «отдыхают», подписывать всё равно надо - иначе на чате
    /// из одного человека фича молча выключается.
    #[test]
    fn cooldown_never_blocks_everything() {
        let mut a = NicknameAssigner::default();
        let now = Instant::now();
        let people = viewers(&["solo"]);
        assert_eq!(a.assign(&[handle(1)], &people, 1, now), vec![Some("solo".to_string())]);
        assert_eq!(
            a.assign(&[handle(2)], &people, 1, now + Duration::from_secs(1)),
            vec![Some("solo".to_string())],
            "в кадре один враг - единственный зритель обязан его подписать"
        );
    }

    /// Врага давно не видно - запись о нём не должна копиться вечно.
    #[test]
    fn forgotten_enemies_are_evicted() {
        let mut a = NicknameAssigner::default();
        let now = Instant::now();
        a.assign(&[handle(1)], &viewers(&["alpha"]), 1, now);
        assert_eq!(a.assigned.len(), 1);
        a.assign(&[], &viewers(&["alpha"]), 1, now + FORGET_AFTER + Duration::from_secs(1));
        assert!(a.assigned.is_empty());
    }

    /// Масштаб интерфейса должен быть равномерным, а остаток уходить в поля.
    ///
    /// 16:9 любого размера - без полей; всё остальное - с полями по той оси,
    /// где экран «лишний». Иначе сдвиг подписи и её кегль разъезжаются.
    #[test]
    fn projection_puts_a_point_where_the_camera_looks() {
        // Камеру из игры в тесте не достать, поэтому проверяем саму формулу
        // экранного преобразования: центр кадра, край и точка позади камеры.
        let screen = [1920.0, 1080.0];
        let ndc = |x: f32, y: f32| [(x * 0.5 + 0.5) * screen[0], (1.0 - (y * 0.5 + 0.5)) * screen[1]];
        assert_eq!(ndc(0.0, 0.0), [960.0, 540.0]);
        // Выше по вертикали в мире - выше на экране (меньше y в пикселях).
        assert!(ndc(0.0, 0.5)[1] < ndc(0.0, 0.0)[1]);
        // Правее в мире - правее на экране.
        assert!(ndc(0.5, 0.0)[0] > ndc(0.0, 0.0)[0]);
    }

    #[test]
    fn ui_scale_keeps_aspect() {
        let (k, pad) = ui_scale([1920.0, 1080.0]);
        assert_eq!((k, pad), (1.0, [0.0, 0.0]));

        // 2K и 4K - те же 16:9, только крупнее.
        let (k, pad) = ui_scale([2560.0, 1440.0]);
        assert!((k - 1.3333334).abs() < 1e-5, "k = {k}");
        assert_eq!(pad, [0.0, 0.0]);

        // Ultrawide: масштаб по высоте, лишняя ширина - в поля по бокам.
        let (k, pad) = ui_scale([2560.0, 1080.0]);
        assert_eq!(k, 1.0);
        assert_eq!(pad, [320.0, 0.0]);

        // 16:10: наоборот, лишняя высота уходит вверх и вниз.
        let (k, pad) = ui_scale([1920.0, 1200.0]);
        assert_eq!(k, 1.0);
        assert_eq!(pad, [0.0, 60.0]);
    }

    /// Без зрителей подписывать нечем - и это не повод падать.
    #[test]
    fn no_viewers_means_no_names() {
        let mut a = NicknameAssigner::default();
        assert!(a.assign(&[handle(1)], &[], 1, Instant::now()).is_empty());
    }

}
