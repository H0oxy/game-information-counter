//! Блокировка игрового ввода, пока открыто окно настроек.
//!
//! Единственное место в моде, где мы что-то патчим в чужом коде, — до
//! появления окна настроек мод был чисто читающим. Перед любой правкой
//! здесь читать список ниже целиком: две попытки "улучшить" это место уже
//! были, и обе откачены в тот же день.
//!
//! Коротко, что нельзя делать:
//!
//! - **не добавлять проверку "а не пропатчено ли уже"**. В живой игре все три
//!   цели уже пропатчены — Steam-оверлей успевает раньше, — и такая проверка
//!   молча не устанавливает ничего. Стоять поверх чужого трамплина здесь
//!   нормально и работает годами;
//! - **не замораживать потоки вокруг патча**. `retour::enable()` внутри
//!   аллоцирует (`region::protect_with_handle` собирает `Vec`), а
//!   `SuspendThread` на потоке, держащем кучу, — гарантированный дедлок без
//!   шансов на восстановление.
//!
//! Измерено в elden живьём: игра читает ввод **только** через
//! `GetRawInputData` (7732 вызова за сессию против 0 у `GetRawInputBuffer`),
//! поэтому буферный вариант тут не хукается вовсе.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use retour::GenericDetour;

/// До какого момента (мс от старта мода) ввод принадлежит нашему UI.
/// Штамп времени, а не флаг: если render перестанет вызываться с открытым
/// окном, блокировка снимется сама, а не запрёт управление навсегда.
static UI_CAPTURE_UNTIL_MS: AtomicU64 = AtomicU64::new(0);

/// Насколько вперёд каждый кадр продлевается захват ввода.
const CAPTURE_LEASE_MS: u64 = 250;

fn uptime_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Вызывается каждый кадр, пока окно настроек открыто.
pub fn hold_capture() {
    UI_CAPTURE_UNTIL_MS.store(uptime_ms() + CAPTURE_LEASE_MS, Ordering::Relaxed);
}

pub fn release_capture() {
    UI_CAPTURE_UNTIL_MS.store(0, Ordering::Relaxed);
}

fn capturing() -> bool {
    let until = UI_CAPTURE_UNTIL_MS.load(Ordering::Relaxed);
    until != 0 && uptime_ms() < until
}

type GetRawInputDataFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut u32, u32) -> u32;
type SetCursorPosFn = unsafe extern "system" fn(i32, i32) -> i32;
type ClipCursorFn = unsafe extern "system" fn(*const c_void) -> i32;

static RAW_INPUT_DATA_HOOK: OnceLock<GenericDetour<GetRawInputDataFn>> = OnceLock::new();
static SET_CURSOR_POS_HOOK: OnceLock<GenericDetour<SetCursorPosFn>> = OnceLock::new();
static CLIP_CURSOR_HOOK: OnceLock<GenericDetour<ClipCursorFn>> = OnceLock::new();

const RID_INPUT: u32 = 0x1000_0003;

// Прятать от игры ВСЁ - значит защёлкнуть то, что было зажато в момент
// открытия окна: бежишь вперёд, жмёшь F7, и отпускание W до игры уже не
// доходит - персонаж бежит, пока alt-tab не сбросит игре состояние ввода
// (жалоба; в elden та же проблема и починена тем же способом). Поэтому
// событие, которое только ОТПУСКАЕТ, пропускаем; нажатия, движение мыши и
// колесо по-прежнему глушим.
//
// Заодно это чинит вторую половину той же проблемы: собственный `key_up` мода
// (`actions::release_all` на открытии окна) идёт через `SendInput` тем же
// путём, и без пропуска отпусканий терялся ровно так же.
//
// x64 RAWINPUT: заголовок {dwType u32, dwSize u32, hDevice u64, wParam u64} =
// 24 байта, дальше RAWKEYBOARD {MakeCode u16, Flags u16, ...} или RAWMOUSE
// {usFlags u16, паддинг, usButtonFlags u16, usButtonData u16, ulRawButtons
// u32, lLastX/Y i32, ...}.
const RIM_TYPEMOUSE: u32 = 0;
const RIM_TYPEKEYBOARD: u32 = 1;
const RI_KEY_BREAK: u16 = 1;
const MOUSE_BUTTON_UP_MASK: u16 = 0x0002 | 0x0008 | 0x0020 | 0x0080 | 0x0200;

/// Вычищает из одного `RAWINPUT` всё, кроме отпусканий, прямо на месте.
/// `false` - отдавать игре нечего, событие прячем целиком.
unsafe fn keep_release_only(entry: *mut u8, size: usize) -> bool {
    unsafe {
        match (entry as *const u32).read_unaligned() {
            RIM_TYPEKEYBOARD if size >= 40 => (entry.add(26) as *const u16).read_unaligned() & RI_KEY_BREAK != 0,
            RIM_TYPEMOUSE if size >= 48 => {
                // Отпускание кнопки оставляем, а поворот камеры и колесо,
                // приехавшие в том же событии, обнуляем.
                let flags = entry.add(28) as *mut u16;
                let up = flags.read_unaligned() & MOUSE_BUTTON_UP_MASK;
                flags.write_unaligned(up);
                (entry.add(30) as *mut u16).write_unaligned(0); // usButtonData (колесо)
                (entry.add(32) as *mut u32).write_unaligned(0); // ulRawButtons
                (entry.add(36) as *mut i32).write_unaligned(0); // lLastX
                (entry.add(40) as *mut i32).write_unaligned(0); // lLastY
                up != 0
            }
            _ => false,
        }
    }
}

/// Забирает у игры содержимое ввода, оставляя сам вызов успешным: игра видит
/// "событий нет" вместо ошибки. Отпускания при этом проходят - см. выше.
unsafe extern "system" fn get_raw_input_data_detour(
    h_raw_input: *mut c_void,
    ui_command: u32,
    p_data: *mut c_void,
    pcb_size: *mut u32,
    cb_size_header: u32,
) -> u32 {
    let Some(hook) = RAW_INPUT_DATA_HOOK.get() else {
        return u32::MAX;
    };
    let real = unsafe { hook.call(h_raw_input, ui_command, p_data, pcb_size, cb_size_header) };
    if capturing()
        && ui_command == RID_INPUT
        && !p_data.is_null()
        && real != u32::MAX
        && !unsafe { keep_release_only(p_data.cast(), real as usize) }
    {
        return 0;
    }
    real
}

/// Игра каждый кадр возвращает курсор в центр (обзор мышью). Пока окно
/// открыто - не даём, иначе курсор невозможно навести на ползунок.
unsafe extern "system" fn set_cursor_pos_detour(x: i32, y: i32) -> i32 {
    if capturing() {
        return 1;
    }
    let Some(hook) = SET_CURSOR_POS_HOOK.get() else { return 1 };
    unsafe { hook.call(x, y) }
}

/// То же для области, в которую игра запирает курсор.
unsafe extern "system" fn clip_cursor_detour(rect: *const c_void) -> i32 {
    if capturing() {
        return 1;
    }
    let Some(hook) = CLIP_CURSOR_HOOK.get() else { return 1 };
    unsafe { hook.call(rect) }
}

fn install_hook<F: retour::Function>(cell: &'static OnceLock<GenericDetour<F>>, original: F, detour: F) {
    let Ok(hook) = (unsafe { GenericDetour::new(original, detour) }) else {
        return;
    };
    // При ошибке хук просто дропается и не включается - уже пропатченный код
    // при этом не трогается.
    if cell.set(hook).is_err() {
        return;
    }
    if let Some(hook) = cell.get() {
        let _ = unsafe { hook.enable() };
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleA(name: *const u8) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

#[link(name = "user32")]
extern "system" {
    /// `POINT` - два `LONG`, то же самое, что `[i32; 2]`.
    fn GetCursorPos(point: *mut [i32; 2]) -> i32;

    fn OpenClipboard(owner: *mut c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn GetClipboardData(format: u32) -> *mut c_void;
    fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
}

#[link(name = "kernel32")]
extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalFree(mem: *mut c_void) -> *mut c_void;
    fn GlobalLock(mem: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(mem: *mut c_void) -> i32;
}

const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

/// Буфер обмена для ImGui.
///
/// Без него `Ctrl+V` в поле ввода молча не работает: imgui-rs не ставит
/// никакого обработчика сам, а игра свой не предоставляет. Первым это поймал
/// пользователь на поле Client ID - вставить туда 30 случайных символов было
/// нечем, только набирать руками.
pub struct Clipboard;

impl hudhook::imgui::ClipboardBackend for Clipboard {
    fn get(&mut self) -> Option<String> {
        unsafe {
            // Буфером владеет другое окно - это норма, а не ошибка: просто
            // сейчас не наша очередь, вернём пусто.
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return None;
            }
            let text = read_unicode_text();
            CloseClipboard();
            text
        }
    }

    fn set(&mut self, value: &str) {
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return;
            }
            write_unicode_text(value);
            CloseClipboard();
        }
    }
}

/// Читает `CF_UNICODETEXT` из уже открытого буфера.
unsafe fn read_unicode_text() -> Option<String> {
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
    if handle.is_null() {
        return None;
    }
    let ptr = unsafe { GlobalLock(handle) } as *const u16;
    if ptr.is_null() {
        return None;
    }
    // Длину даёт нулевой символ: размер блока может быть больше строки.
    let mut len = 0usize;
    // Предохранитель от блока без нуля - лучше обрезать, чем читать чужую
    // память до первого совпадения.
    const MAX: usize = 64 * 1024;
    while len < MAX && unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) });
    unsafe { GlobalUnlock(handle) };
    Some(text)
}

/// Кладёт строку в уже открытый буфер как `CF_UNICODETEXT`.
unsafe fn write_unicode_text(value: &str) {
    let mut utf16: Vec<u16> = value.encode_utf16().collect();
    utf16.push(0);
    let bytes = utf16.len() * std::mem::size_of::<u16>();
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
    if handle.is_null() {
        return;
    }
    let dst = unsafe { GlobalLock(handle) } as *mut u16;
    if dst.is_null() {
        unsafe { GlobalFree(handle) };
        return;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len());
        GlobalUnlock(handle);
        EmptyClipboard();
        // После успешного `SetClipboardData` память принадлежит системе, и
        // освобождать её нельзя. При неудаче - наоборот, освободить обязаны.
        if SetClipboardData(CF_UNICODETEXT, handle).is_null() {
            GlobalFree(handle);
        }
    }
}

/// Текущая позиция курсора в координатах экрана. Нужна `xy_pad` в `overlay.rs`
/// для относительного перетаскивания (см. `overlay.rs`).
pub fn cursor_pos() -> [i32; 2] {
    let mut p = [0i32; 2];
    unsafe { GetCursorPos(&mut p) };
    p
}

/// Ставит курсор, минуя `set_cursor_pos_detour`: тот глушит любые вызовы
/// `SetCursorPos`, пока идёт захват ввода, включая наши собственные. Как
/// `release_cursor_clip` - зовёт хук напрямую, в обход своего же детура.
pub fn pin_cursor(p: [i32; 2]) {
    if let Some(hook) = SET_CURSOR_POS_HOOK.get() {
        unsafe { hook.call(p[0], p[1]) };
    }
}

/// Снимает область, в которую игра заперла курсор ДО того, как окно открылось.
/// Детур ловит только новые вызовы, а уже установленный `ClipCursor` держит
/// курсор в центре экрана, и мышью не дотянуться до половины окна.
pub fn release_cursor_clip() {
    // Мимо нашего детура: он бы просто вернул успех, ничего не сняв.
    if let Some(hook) = CLIP_CURSOR_HOOK.get() {
        let _ = unsafe { hook.call(std::ptr::null()) };
    }
}

pub fn install() {
    unsafe {
        let user32 = GetModuleHandleA(c"user32.dll".as_ptr().cast());
        if user32.is_null() {
            return;
        }
        let export = |name: &std::ffi::CStr| {
            let addr = GetProcAddress(user32, name.as_ptr().cast());
            (!addr.is_null()).then_some(addr)
        };
        if let Some(a) = export(c"GetRawInputData") {
            install_hook(&RAW_INPUT_DATA_HOOK, std::mem::transmute::<_, GetRawInputDataFn>(a), get_raw_input_data_detour);
        }
        if let Some(a) = export(c"SetCursorPos") {
            install_hook(&SET_CURSOR_POS_HOOK, std::mem::transmute::<_, SetCursorPosFn>(a), set_cursor_pos_detour);
        }
        if let Some(a) = export(c"ClipCursor") {
            install_hook(&CLIP_CURSOR_HOOK, std::mem::transmute::<_, ClipCursorFn>(a), clip_cursor_detour);
        }
    }
}

/// Снимает детуры. Вызывается только из `DllMain(DLL_PROCESS_DETACH)` при
/// явной выгрузке: тела детуров живут в этой DLL, и оставить пропатченный
/// пролог, ведущий в выгруженный модуль, - гарантированное падение на
/// следующем движении мыши.
///
/// Гонку с потоком, который прямо сейчас *внутри* детура, это не закрывает
/// (для этого нужен счётчик входов), но пропатченный пролог - это
/// достоверность, а не гонка.
///
/// Здесь бежит код под loader lock, а `disable()` внутри аллоцирует. Ничего
/// сверх одного вызова на хук сюда добавлять нельзя - ни логирования, ни
/// перечисления потоков.
pub fn uninstall() {
    macro_rules! off {
        ($cell:expr) => {
            if let Some(h) = $cell.get() {
                let _ = unsafe { h.disable() };
            }
        };
    }
    off!(RAW_INPUT_DATA_HOOK);
    off!(SET_CURSOR_POS_HOOK);
    off!(CLIP_CURSOR_HOOK);
}

// ---------------------------------------------------------------------------
// Синтетический ввод: зрители нажимают клавиши за баллы канала
// ---------------------------------------------------------------------------
//
// `SendInput` - обычный системный вызов, а не патч чужого кода: никакого
// детура он не требует, и read-only природы мода не нарушает. Игра читает
// ввод через `GetRawInputData`, а туда синтетические события Windows кладёт
// наравне с физическими - то есть отдельного пути для них не нужно.
//
// НЕ ПРОВЕРЕНО ЖИВЬЁМ: что Elden Ring действительно принимает синтетические
// события. По документации Win32 raw input их получает, но конкретно эта игра
// может фильтровать - проверять первым делом кнопкой «тестовая покупка».

/// `KEYBDINPUT` внутри `INPUT`. Раскладку не трогаем: шлём скан-код, а не
/// виртуальную клавишу, иначе на не-QWERTY раскладке W уехал бы в Ц.
#[repr(C)]
struct KeybdInput {
    vk: u16,
    scan: u16,
    flags: u32,
    time: u32,
    extra: usize,
}

#[repr(C)]
struct Input {
    kind: u32,
    ki: KeybdInput,
    // `INPUT` - это union из трёх структур, и самая большая (MOUSEINPUT)
    // длиннее KEYBDINPUT. Хвост обязателен, иначе SendInput прочитает мусор.
    tail: [u8; 8],
}

const INPUT_KEYBOARD: u32 = 1;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_SCANCODE: u32 = 0x0008;
const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const MAPVK_VK_TO_VSC: u32 = 0;

#[link(name = "user32")]
extern "system" {
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
    fn MapVirtualKeyW(code: u32, map_type: u32) -> u32;
    fn GetAsyncKeyState(vk: i32) -> i16;
    fn GetForegroundWindow() -> usize;
    fn GetWindowThreadProcessId(hwnd: usize, pid: *mut u32) -> u32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcessId() -> u32;
}

/// Стоит ли фокус на окне игры прямо сейчас.
///
/// `SendInput` бьёт не по игре, а по АКТИВНОМУ окну всей системы: пока стример
/// сидит в браузере, купленная зрителем клавиша уходит туда. Поэтому купленное
/// исполняется только при своём фокусе (см. `gameplay_active` в lib.rs).
///
/// Сравниваем процессы, а не хэндлы: у игры не одно окно (Steam-оверлей
/// заводит свои), и владелец фокуса среди них может быть любым.
pub fn game_focused() -> bool {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd == 0 {
        return false;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    pid != 0 && pid == unsafe { GetCurrentProcessId() }
}

/// Нажата ли клавиша прямо сейчас физически.
///
/// Нужно, чтобы купленное зрителем удержание не обрывало игрока: если он уже
/// держит W сам, наш `key_up` через три секунды остановит его бег - для игрока
/// это выглядит как «мод отпустил мне клавишу». Такую клавишу не трогаем вовсе
/// (см. `actions::hold`).
///
/// ponytail: собственное нажатие мода этот флаг тоже поднимает, различить их
/// нечем. Работает потому, что спрашиваем только ПЕРЕД своим нажатием.
pub fn physically_down(vk: u16) -> bool {
    (unsafe { GetAsyncKeyState(vk as i32) } as u16) & 0x8000 != 0
}

fn send_key(vk: u16, up: bool) {
    // Под `cargo test` не шлём ничего. `SendInput` бьёт по настоящей
    // клавиатуре всей системы, а тесты очереди (`actions::tests`) нажимают W,
    // A и пробел - и это уезжало в то окно, где в тот момент стоял фокус:
    // «сами нажимаются ц ц ф» (жалоба 2026-08-20; в русской раскладке W и A
    // это как раз ц и ф). Проверять тут нечего: интересна логика очереди и
    // списка `held`, а не то, дошёл ли системный вызов.
    if cfg!(test) {
        return;
    }
    // Скан-код игра ждёт от raw input; получаем его из виртуальной клавиши
    // текущей раскладкой.
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) };
    // Расширенность - из явного списка, а НЕ из старшего байта скан-кода.
    // `MAPVK_VK_TO_VSC_EX` обещает вернуть там 0xE0, но для стрелок и
    // навигационного блока не возвращает: замер 2026-08-19 дал у стрелки вверх
    // 0x0048 - тот же скан-код, что у Num 8. Игра получала Num 8 вместо
    // стрелки, и снаружи это выглядело как «стрелки не нажимаются вовсе».
    let extended = crate::twitch::rewards::EXTENDED_VKS.contains(&vk);
    let mut flags = KEYEVENTF_SCANCODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    let input = Input {
        kind: INPUT_KEYBOARD,
        ki: KeybdInput { vk: 0, scan: (scan & 0xFF) as u16, flags, time: 0, extra: 0 },
        tail: [0; 8],
    };
    unsafe { SendInput(1, &input, std::mem::size_of::<Input>() as i32) };
}

pub fn key_down(vk: u16) {
    send_key(vk, false);
}

pub fn key_up(vk: u16) {
    send_key(vk, true);
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: *mut c_void,
        op: *const u16,
        file: *const u16,
        params: *const u16,
        dir: *const u16,
        show: i32,
    ) -> *mut c_void;
}

/// Открывает ссылку в браузере по умолчанию.
///
/// Нужно, чтобы адреса в окне настроек были нажимаемыми: переписывать
/// `dev.twitch.tv/console/apps` с экрана руками - худшее, что можно предложить
/// человеку, который уже сидит в игре.
pub fn open_url(url: &str) {
    // Пускаем только http(s): строка приходит из нашего же кода, но
    // `ShellExecute` запустил бы и исполняемый файл, так что проверка тут
    // дешевле раздумий о том, кто ещё может её вызвать.
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return;
    }
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (op, file) = (wide("open"), wide(url));
    const SW_SHOWNORMAL: i32 = 1;
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Починка залипшей клавиши: нажатие прячем, отпускание отдаём игре.
    #[test]
    fn only_releases_reach_the_game() {
        // `Vec<u64>` ради восьмибайтового выравнивания, как у настоящего
        // буфера raw input.
        let mut backing = vec![0u64; 6];
        let buf = unsafe { std::slice::from_raw_parts_mut(backing.as_mut_ptr().cast::<u8>(), 48) };
        let put = |buf: &mut [u8], flags: u16| {
            buf[0..4].copy_from_slice(&RIM_TYPEKEYBOARD.to_le_bytes());
            buf[4..8].copy_from_slice(&40u32.to_le_bytes());
            buf[26..28].copy_from_slice(&flags.to_le_bytes());
            buf[30..32].copy_from_slice(&0x57u16.to_le_bytes()); // W
        };

        put(buf, 0);
        assert!(!unsafe { keep_release_only(buf.as_mut_ptr(), 40) }, "нажатие игре не отдаём");
        put(buf, RI_KEY_BREAK);
        assert!(unsafe { keep_release_only(buf.as_mut_ptr(), 40) }, "отпускание обязано пройти");

        // Обрезанное событие разбирать нечем - прячем целиком.
        assert!(!unsafe { keep_release_only(buf.as_mut_ptr(), 8) });
    }

    /// Мышь: отпускание кнопки проходит, но поворот камеры вместе с ним - нет.
    #[test]
    fn mouse_release_carries_no_movement() {
        let mut backing = vec![0u64; 6];
        let buf = unsafe { std::slice::from_raw_parts_mut(backing.as_mut_ptr().cast::<u8>(), 48) };
        buf[0..4].copy_from_slice(&RIM_TYPEMOUSE.to_le_bytes());
        buf[4..8].copy_from_slice(&48u32.to_le_bytes());
        buf[28..30].copy_from_slice(&(0x0001u16 | 0x0002).to_le_bytes()); // левая: нажата и отпущена
        buf[36..40].copy_from_slice(&40i32.to_le_bytes()); // lLastX
        buf[40..44].copy_from_slice(&(-25i32).to_le_bytes()); // lLastY
        assert!(unsafe { keep_release_only(buf.as_mut_ptr(), 48) });
        assert_eq!(u16::from_le_bytes([buf[28], buf[29]]), 0x0002, "осталось только отпускание");
        assert_eq!(i32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]), 0);
        assert_eq!(i32::from_le_bytes([buf[40], buf[41], buf[42], buf[43]]), 0);
    }
}
