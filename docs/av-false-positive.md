# False positive reports

Пять форм, потому что независимых движков пять, а не семь:
Avast=AVG, Avira=WithSecure.

| Вендор | Форма |
|---|---|
| Microsoft | https://microsoft.com/wdsi/filesubmission (выбрать "Software developer") |
| Avast / AVG | https://www.avast.com/false-positive-file-form.php |
| Avira / WithSecure | https://www.avira.com/en/analysis/submit |
| AhnLab | https://global.ahnlab.com → Support → False positive |
| Cynet | через support-форму на cynet.com |

Текст обращения (английский, один на всех):

---
Subject: False positive: game_information_counter.dll (SHA256 12f5b11875d16154c90b504b7315695fe8098f5cd0aeeb5039fa23c0ca3cea51)

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

The file contains no process injection into foreign processes, no
anti-debugging or anti-VM, no packer or obfuscation, and no persistence
mechanism. Sources: <ссылка на репозиторий>

Please whitelist. Thank you.
---

После репортов: Reanalyze на VirusTotal через несколько дней. Microsoft и
пара Avast/AVG реагируют быстрее прочих.
