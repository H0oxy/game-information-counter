//! Метаданные DLL (VERSIONINFO).
//!
//! Единственное, что можно сделать против ложных срабатываний антивирусов, не
//! отказываясь от функций мода: DLL без имени, версии и описания - для
//! эвристики признак сама по себе, а у нас внутри детуры user32, `SendInput` и
//! сеть, то есть набор, который в отчёте выглядит как «инжектор с
//! кейлоггером». Настоящее лечение - подпись сертификатом, но она платная и
//! ставится не здесь.
//!
//! Ошибка компиляции ресурса не роняет сборку: `rc.exe` есть не в каждой
//! установке Build Tools, а мод от отсутствия метаданных не перестаёт
//! работать.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set("ProductName", "Game Information Counter")
        .set("FileDescription", "Game Information Counter - stats overlay and viewer interaction for Elden Ring")
        .set("CompanyName", "Game Information Counter")
        .set("LegalCopyright", "MIT License")
        .set("OriginalFilename", "game_information_counter.dll")
        .set("InternalName", "game_information_counter");
    if let Err(e) = res.compile() {
        println!("cargo:warning=ресурс версии не собран ({e}) - DLL выйдет без метаданных");
    }
}
