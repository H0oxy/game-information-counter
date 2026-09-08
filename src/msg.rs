//! Чтение игровых текстов (FMG) прямо из памяти.
//!
//! Игра держит все строки в `MsgRepositoryImp`: массив слотов, в каждом -
//! буфер FMG. Крейт отдаёт только указатель на сам репозиторий (структура в нём
//! пустая заглушка), поэтому навигация до буфера и разбор формата - наши.
//!
//! Цепочка и числа сняты с рабочего мода `VirusAlex/ERR-MapForGoblins-DLL`
//! (`src/goblin_messages.cpp`), а не угаданы. Там же выяснилось, что AOB-скан,
//! которым он ищет репозиторий, нам не нужен вовсе: у нас этот синглтон уже
//! есть через рефлексию Dantelion2.
//!
//! Прошлый заход (удалённый `src/msg.rs`, 2026-08-20) упирался ровно в это:
//! `repo + 0x08` был верен, но не хватало второго шага (`base[0]`) и номера
//! слота.
//!
//! Только чтение. Ничего не пишем, игровых функций не зовём.

use eldenring::cs::MsgRepositoryImp;
use fromsoftware_shared::FromStatic;

/// Имена врагов и боссов. Подтверждено живьём 2026-08-27: резолв `fmg_id`
/// боссовой полоски через этот слот дословно совпал с именем, которое игра
/// нарисовала сама.
///
/// Остальные слоты (PlaceName 19, TutorialTitle 207 и прочие) в коде не
/// перечислены намеренно: незнакомый text id дешевле искать по всем слотам
/// (`where_is`), чем гадать. DLC-слои (`+310`/`+410`) не трогаем - у
/// MapForGoblins там оказались протухшие указатели, и хождение по ним валило
/// игру.
/// Названия мест, и там же - имена боссовых маркеров на карте.
///
/// Имена врагов и боссов лежат в слоте 18 (NpcName): проверено живьём
/// 2026-08-27, резолв `fmg_id` боссовой полоски через него дословно совпал с
/// именем, которое игра нарисовала сама. Здесь он не нужен - к строке
/// `GameAreaParam` привязан именно маркер карты, а он в PlaceName.
pub const PLACE_NAME: usize = 19;

/// Имена врагов и боссов. Проверено живьём 2026-08-27: резолв `fmg_id`
/// боссовой полоски через этот слот дословно совпал с именем, которое игра
/// нарисовала сама.
pub const NPC_NAME: usize = 18;

/// Строка в тексте не бывает длиннее этого. Потолок на случай, если мы всё-таки
/// пришли не туда: без него поиск нуля ушёл бы гулять по чужой памяти.
pub(crate) const MAX_CHARS: usize = 512;

/// Смещения, которые в формате FMG хранятся числом, а не указателем, всегда
/// меньше этого. Больше - значит там уже абсолютный адрес (игра чинит буфер при
/// загрузке). Ровно тот же порог, что у MapForGoblins.
const ABSOLUTE: u64 = 0x100_0000;

/// Группа идущих подряд id в буфере FMG.
#[repr(C)]
struct Group {
    string_index: i32,
    first_id: i32,
    last_id: i32,
    _pad: i32,
}

/// Буфер FMG нужного слота.
///
/// ## Safety
/// Зовётся только с игрового потока (`render`), как и всё остальное чтение.
unsafe fn slots() -> Option<(*const *const u8, usize)> {
    let repo = MsgRepositoryImp::instance_ptr().ok()? as *const u8;
    if repo.is_null() {
        return None;
    }
    let base = (repo.add(0x08) as *const *const *const u8).read_unaligned();
    let count = (repo.add(0x14) as *const i32).read_unaligned();
    if base.is_null() || count <= 0 {
        return None;
    }
    // base[0] - это массив указателей на буферы, а не буфер.
    let sub = base.read_unaligned() as *const *const u8;
    (!sub.is_null()).then_some((sub, count as usize))
}

/// Буфер FMG нужного слота.
///
/// ## Safety
/// Зовётся только с игрового потока (`render`), как и всё остальное чтение.
unsafe fn fmg_of(slot: usize) -> Option<*const u8> {
    let (sub, count) = slots()?;
    if slot >= count {
        return None;
    }
    let fmg = sub.add(slot).read_unaligned();
    (!fmg.is_null()).then_some(fmg)
}

/// Заголовок буфера FMG: групп, строк, где таблица смещений.
///
/// `None` - слот пустой или протухший. Без этой проверки обход групп уходит за
/// пределы всего; у MapForGoblins она появилась после падения игры.
///
/// ## Safety
/// `fmg` - буфер из [`fmg_of`] либо любой читаемый адрес.
unsafe fn header(fmg: *const u8) -> Option<(u32, u32, u64)> {
    if fmg.is_null() {
        return None;
    }
    let group_cnt = (fmg.add(0x0C) as *const u32).read_unaligned();
    let string_cnt = (fmg.add(0x10) as *const u32).read_unaligned();
    let raw = (fmg.add(0x18) as *const u64).read_unaligned();
    (group_cnt != 0 && group_cnt <= 0x10_0000 && string_cnt <= 0x20_0000 && raw != 0)
        .then_some((group_cnt, string_cnt, raw))
}

/// Строка по id внутри одного буфера FMG.
///
/// ## Safety
/// `fmg` должен быть буфером, полученным из [`fmg_of`].
unsafe fn lookup(fmg: *const u8, id: i32) -> Option<String> {
    if fmg.is_null() || id <= 0 {
        return None;
    }
    let (group_cnt, string_cnt, raw) = header(fmg)?;
    let offsets = if raw > ABSOLUTE { raw as *const u64 } else { fmg.add(raw as usize) as *const u64 };
    let groups = fmg.add(0x28) as *const Group;

    for g in 0..group_cnt as usize {
        let gr = &*groups.add(g);
        if id < gr.first_id || id > gr.last_id {
            continue;
        }
        let si = gr.string_index + (id - gr.first_id);
        if si < 0 || si >= string_cnt as i32 {
            return None;
        }
        let off = offsets.add(si as usize).read_unaligned();
        if off == 0 {
            return None;
        }
        let s = if off > ABSOLUTE { off as *const u16 } else { fmg.add(off as usize) as *const u16 };
        return utf16(s);
    }
    None
}

/// UTF-16 до нуля. Пустая строка - это "записи нет", а не пустое имя.
pub(crate) unsafe fn utf16(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut chars = Vec::new();
    for i in 0..MAX_CHARS {
        let c = p.add(i).read_unaligned();
        if c == 0 {
            break;
        }
        chars.push(c);
    }
    (!chars.is_empty()).then(|| String::from_utf16_lossy(&chars))
}

/// Строка игры по слоту и id.
pub fn text(slot: usize, id: i32) -> Option<String> {
    unsafe { lookup(fmg_of(slot)?, id) }
}

/// Строка по id в первом слоте, где она нашлась.
///
/// Для текстов, слот которых заранее неизвестен: у меню телепорта они лежат в
/// 368 и 200, а не в 19 вместе с названиями мест (замерено живьём 2026-08-27),
/// и зашивать эти номера значило бы поверить, что они те же на всех версиях
/// игры и со всеми модами. Проход по всем слотам стоит одного резолва
/// синглтона и обхода групп - это терпимо там, где зовётся один раз при
/// постройке реестра, и НЕ годится для того, что считается каждый кадр.
pub fn text_anywhere(id: i32) -> Option<String> {
    if id <= 0 {
        return None;
    }
    unsafe {
        let (sub, count) = slots()?;
        (0..count).find_map(|s| lookup(sub.add(s).read_unaligned(), id))
    }
}

/// То же, но с DLC-слоями поверх базового.
///
/// Тексты Земель Теней лежат отдельным слоем (`+310`/`+410` к базовому слоту),
/// и в базовом их нет вовсе: живьём 2026-08-27 боссы карт `m61_*` остались без
/// имени региона именно поэтому. MapForGoblins эти слои не трогает - у его
/// загрузчика там протухшие указатели, - но `lookup` от мусора защищён
/// заголовком, так что попробовать дешевле, чем отказаться.
pub fn text_layered(slot: usize, id: i32) -> Option<String> {
    text(slot, id).or_else(|| text(slot + 310, id)).or_else(|| text(slot + 410, id))
}

// ---------------------------------------------------------------------------
// Диагностика (этап 1)
// ---------------------------------------------------------------------------
//
// Временная: живёт, пока не станет ясно, что именно резолвится. Правило
// проекта - тянуться к счётчику на экране раньше, чем кажется оправданным;
// обе прошлые смерти босс-листа это ровно пропущенный такой шаг.


#[cfg(test)]
mod tests {
    use super::*;

    /// Синтетический буфер FMG: заголовок, одна группа на два id, две строки.
    /// Ловит арифметику групп и смещений - ровно то, ради чего файл и написан.
    fn buffer() -> Vec<u8> {
        let mut b = vec![0u8; 0x80];
        b[0x0C..0x10].copy_from_slice(&1u32.to_le_bytes()); // групп
        b[0x10..0x14].copy_from_slice(&2u32.to_le_bytes()); // строк
        b[0x18..0x20].copy_from_slice(&0x40u64.to_le_bytes()); // таблица смещений
        // Группа: id 100..101 -> строки 0..1.
        b[0x28..0x2C].copy_from_slice(&0i32.to_le_bytes());
        b[0x2C..0x30].copy_from_slice(&100i32.to_le_bytes());
        b[0x30..0x34].copy_from_slice(&101i32.to_le_bytes());
        b[0x40..0x48].copy_from_slice(&0x50u64.to_le_bytes());
        b[0x48..0x50].copy_from_slice(&0x60u64.to_le_bytes());
        for (i, c) in "Маления".encode_utf16().enumerate() {
            b[0x50 + i * 2..0x52 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        for (i, c) in "Godrick".encode_utf16().enumerate() {
            b[0x60 + i * 2..0x62 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        b
    }

    #[test]
    fn walks_groups_and_offsets() {
        let b = buffer();
        let p = b.as_ptr();
        assert_eq!(unsafe { lookup(p, 100) }.as_deref(), Some("Маления"));
        assert_eq!(unsafe { lookup(p, 101) }.as_deref(), Some("Godrick"));
        assert_eq!(unsafe { lookup(p, 99) }, None, "id вне группы");
        assert_eq!(unsafe { lookup(p, 102) }, None);
        assert_eq!(unsafe { lookup(p, 0) }, None);
    }

    #[test]
    fn implausible_header_is_not_walked() {
        let mut b = buffer();
        b[0x0C..0x10].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // Без guard'а этот обход ушёл бы за пределы буфера - то есть в чужую
        // память живой игры.
        assert_eq!(unsafe { lookup(b.as_ptr(), 100) }, None);

        let mut b = buffer();
        b[0x18..0x20].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(unsafe { lookup(b.as_ptr(), 100) }, None);
    }
}

