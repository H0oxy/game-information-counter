# False positive reports

Пять адресов, потому что независимых движков пять, а не семь:
Avast=AVG, Avira=WithSecure. AhnLab и Cynet своей формы для файла не
держат вовсе - шлём письмом.

| Вендор | Вердикт (VT, 2026-09-07) | Куда |
|---|---|---|
| Microsoft | `Trojan:Win32/Wacatac.C!ml` | https://www.microsoft.com/en-us/wdsi/filesubmission - Submission type "Software developer", файл + текст ниже |
| Avast / AVG | `Win64:MalwareX-gen [Misc]` | https://www.avast.com/report-false-positive - файл + текст ниже |
| Avira / WithSecure | `TR/W64.Agent` / `Trojan.TR/W64.Agent` | https://www.avira.com/en/analysis/submit - файл, тип "False Positive" |
| AhnLab | `Trojan/Win.Generic.C5936529` | письмо на `v3sos@ahnlab.com`, тема "False positive report", файл в ZIP (их требование) |
| Cynet | `Malicious (score: 100)` | https://www.cynet.com/contact-us/ , тема "False Positive Report" |

Проверено вручную 2026-09-03: у AhnLab и Cynet формы для одиночного файла нет
или не открывается напрямую - оба идут письмом/контакт-формой. Остальные три
рабочие.

**2026-09-07: в выдаче названы только Microsoft и Cynet.** Три остальных
вендора после прошлых репортов файл не помечают - значит репорты работают, и
слать надо ровно тех, кто в текущей выдаче. Вердикт Microsoft сменился с
`.B!ml` на `.C!ml`: это та же ML-эвристика, просто другая её ветка.

**`!ml` в имени и есть ответ на «почему в прошлый раз не ловилось».** Такой
вердикт ставит не сигнатура, а модель, и решает она по неподписанному файлу
без репутации. Каждая сборка - новый хеш и новый ноль репутации, поэтому один
и тот же код проходит в одну сборку и ловится в другую. Пересборка ничего не
чинит, а вот подпись сертификатом чинит - см. запись про VirusTotal в
CLAUDE.md.

Вердикт из второй колонки подставляется в письмо вместо `<вердикт>`, а SHA256
в теме - от ТОГО файла, который отправляется: он меняется с каждой сборкой, и
репорт по чужому хешу вендор просто не свяжет с присланным файлом.

Текст обращения (английский, один на всех):

---
Subject: False positive: game_information_counter.dll (SHA256 2cc3be45449464926cba91e86a5db5c3fd7f924c1c2821275d5a766b428b7ece)

This file is an open-source statistics overlay for the game Elden Ring. It is
loaded into the game process by a standard mod loader (Elden Mod Loader /
ModEngine2) and draws an in-game HUD, plus an optional local web page for OBS.

Your engine flags it as <вердикт>. This is a false positive. The behaviour
that likely triggers the heuristic is all legitimate and documented:

- user32 inline hooks (GetRawInputData, SetCursorPos, ClipCursor) - used only
  to suppress game input while the mod's own settings window is open;
- SendInput / GetAsyncKeyState - the mod presses keys on behalf of Twitch
  channel-point redemptions, and checks whether the player already holds that
  key so it never fights the user;
- clipboard read - Ctrl+V support in the settings window (ImGui provides no
  clipboard backend);
- outbound TLS to id.twitch.tv / api.twitch.tv / eventsub.wss.twitch.tv -
  the official Twitch API, disabled by default;
- a loopback HTTP server on 127.0.0.1 - serves the OBS browser source,
  disabled by default;
- DPAPI (CryptProtectData) - encrypts the user's own Twitch OAuth token at
  rest so it is not stored in plain text.

The file contains no process injection into foreign processes (no
OpenProcess / WriteProcessMemory / CreateRemoteThread in its import table), no
packer or obfuscation, and no persistence mechanism.

Two import groups look suspicious but come from libraries, not from mod logic,
and I would rather point them out than have you find them:

- CreateToolhelp32Snapshot, Thread32First/Next, OpenThread, SuspendThread,
  ResumeThread, GetThreadContext/SetThreadContext, VirtualProtect - these are
  the MinHook hooking library used by hudhook to install the DirectX 12
  present hook that draws the overlay. They enumerate and briefly freeze
  threads of the mod's own process while patching, which is standard for any
  in-process overlay (Steam, Discord and RivaTuner do the same);
- IsDebuggerPresent - imported by the MSVC C runtime, not called by the mod.
  There is no anti-debugging or anti-VM logic in the source. Sources: https://github.com/H0oxy/game-information-counter

Please whitelist. Thank you.
---

После репортов: Reanalyze на VirusTotal через несколько дней. Microsoft и
пара Avast/AVG реагируют быстрее прочих.
