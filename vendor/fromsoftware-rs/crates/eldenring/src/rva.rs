use pelite::pe64::PeView;
use std::sync::LazyLock;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;
use windows::core::PCSTR;

mod bundle;
mod rva_jp;
mod rva_ww;
mod rva_ww_270;
mod rva_ww_271;

pub use bundle::RvaBundle;

use fromsoftware_shared::game_version::{GameVersion, LANG_ID_EN, LANG_ID_JP};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ERGameVersion {
    Ww262,
    Jp2621,
    /// Patch 2.7, worldwide. Mapped locally with `tools/binary-mapper`;
    /// see `vendor/README.md` in the mod repository.
    Ww270,
    /// Patch 2.7.1, worldwide. Mapped locally the same way as 2.7.0.
    Ww271,
}

impl GameVersion for ERGameVersion {
    const NAME: &'static str = "elden ring";

    fn from_lang_version(lang_id: u16, version: &str) -> Option<Self> {
        match (lang_id, version) {
            (LANG_ID_EN, "2.6.2.0") => Some(Self::Ww262),
            (LANG_ID_JP, "2.6.2.1") => Some(Self::Jp2621),
            (LANG_ID_EN, "2.7.0.0") => Some(Self::Ww270),
            (LANG_ID_EN, "2.7.1.0") => Some(Self::Ww271),
            _ => None,
        }
    }
}

impl ERGameVersion {
    const fn rvas(self) -> RvaBundle {
        match self {
            Self::Ww262 => rva_ww::RVAS,
            Self::Jp2621 => rva_jp::RVAS,
            Self::Ww270 => rva_ww_270::RVAS,
            Self::Ww271 => rva_ww_271::RVAS,
        }
    }
}

static RVAS: LazyLock<Result<RvaBundle, String>> = LazyLock::new(|| {
    let module =
        unsafe { PeView::module(GetModuleHandleA(PCSTR(std::ptr::null())).unwrap().0 as *const u8) };
    ERGameVersion::detect(&module)
        .map(|v| v.rvas())
        .map_err(|e| e.to_string())
});

/// The RVA bundle for the current executable, or `None` on a game version this
/// package doesn't know.
///
/// An unknown patch is a normal state, not a bug: the addresses simply haven't
/// been mapped yet. Callers that go through RVAs then fail one by one, while
/// everything resolved through DLRF reflection (by class name) keeps working.
/// This is the entry point new code should use — [`get`] aborts the process for
/// consumers built with `panic = "abort"`, which turns a game update into a
/// crash on startup.
pub fn try_get() -> Option<&'static RvaBundle> {
    RVAS.as_ref().ok()
}

/// Returns the RVA bundle for the current executable region and version.
///
/// This will panic if the current executable isn't supported by this package.
/// Prefer [`try_get`] — see there for why.
pub fn get() -> &'static RvaBundle {
    match RVAS.as_ref() {
        Ok(rvas) => rvas,
        Err(e) => panic!("{e}"),
    }
}
