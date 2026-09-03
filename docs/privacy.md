# Privacy policy

Game Information Counter runs entirely on the user's own machine. It has no
telemetry, no analytics and no update check, and it never sends anything to the
author.

## What stays on the machine

The mod writes these files next to its DLL, and nowhere else:

- `game_information_counter.ini` — settings, including the Twitch Client ID;
- `game_information_counter.stats` — boss attempt counters, per character;
- `game_information_counter.rewards`, `.bosses` — reward setup and boss names
  learned in game;
- `game_information_counter.twitch` — the user's own Twitch OAuth tokens.

The Client ID and the tokens are encrypted at rest with Windows DPAPI, so they
are readable only under the Windows account that saved them.

## What leaves the machine

Only requests to Twitch, and only when the user turns the integration on:

- `id.twitch.tv` — OAuth device flow and token refresh;
- `api.twitch.tv` — the broadcaster's own channel-point rewards and redemptions,
  and the chatter list of the channel the user entered;
- `eventsub.wss.twitch.tv`, `irc-ws.chat.twitch.tv` — live redemptions and chat.

These are the official Twitch APIs, called with the user's own credentials. The
integration is off by default.

The mod also serves a local web page on `127.0.0.1` for OBS Browser Source. It
listens on the loopback interface only, is off by default, and serves two fixed
routes — it never exposes files from disk.

## Data of other people

With viewer nicknames enabled, the mod reads the public chat of the channel the
user entered and shows nicknames (optionally the last chat message) above
enemies on screen. That data lives in memory only, is capped, is dropped when
the channel changes, and is never written to disk or sent anywhere.

## Contact

Issues: https://github.com/H0oxy/game-information-counter/issues
