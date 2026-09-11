# Pubsplash Chat

**The return channel an Icecast broadcast does not have.** One binary, no
database, no accounts, no build step. Copy it to a server, run it, and every
room is live the moment somebody opens its URL.

Icecast carries audio one way. There is no path back from a listener to the
broadcaster — not in the protocol, not in the metadata, not anywhere. So chat
has to be a separate little service running alongside the stream, and this is
the smallest one that is actually pleasant to use with a screen reader.

---

## Quick start

```sh
cargo build --release          # in this directory
./target/release/pubsplash-chat --brand "Night Owl Radio"
```

That is the whole install. It listens on `0.0.0.0:8080`, writes a server secret
to `pubsplash-chat.secret` beside itself on first run, and serves:

| URL | What it is |
| --- | --- |
| `/` | Room picker |
| `/r/nightowl` | The chat page for the room `nightowl` |
| `/r/nightowl/events` | The feed (SSE) |
| `/r/nightowl/messages` | Send one message (`POST`) |
| `/r/nightowl/stream` | Publish where the show can be heard (`PUT`, host key) |
| `/healthz` | `{"ok":true,"rooms":N,"listeners":N}` |

Rooms are **created by being visited**. There is nothing to set up per room and
nothing to clean up: a room with nobody in it is forgotten an hour after its
last message.

Give listeners `https://chat.example.com/r/<your room>` alongside the stream
link and you are done.

## Connecting Pubsplash to it

The broadcaster needs a **host key** so their messages are marked as coming from
the show rather than from another listener. Ask the server for it:

```sh
./pubsplash-chat key nightowl
GRMJDJOCW4TBQ53VIUUB
```

Then in Pubsplash: **Setup streaming services → your Icecast service**, and fill
in Chat server (`https://chat.example.com`), Chat room (`nightowl`) and Chat host
key. Chat then works exactly as it does for an Audio Pub stream — the Chat tab
reads and sends, and messages are spoken by whichever TTS engine is configured.

The key is `HMAC-SHA256(server secret, room name)`, so **nothing is stored per
room** and one deployment serves any number of shows. To revoke every key at
once, change the secret.

## The play button

The chat page carries the show itself: a **Listen button and a volume slider**
(starting at 50%, remembered per browser), so a listener does one thing — open
one link — and can both hear the show and talk about it.

The server cannot know where the show is, so it does not guess. **Pubsplash
publishes the listen address when a stream starts** and clears it when the
stream ends, using the host key. Until it does, the page shows no player at all
— a play button that plays nothing is worse than none. A page that was already
open gains its player the moment the broadcast begins, without a reload.

That means the play button needs the **host key** to be set in Pubsplash. Chat
works fine without one; the radio does not.

Anything else can publish it the same way:

```sh
curl -X PUT https://chat.example.com/r/nightowl/stream \
     -H 'authorization: Bearer GRMJDJOCW4TBQ53VIUUB' \
     -H 'content-type: application/json' \
     -d '{"url":"https://radio.example.com/live.mp3","name":"Night Owl Radio"}'
```

An empty `url` takes the player down. Only `http` and `https` are accepted,
because the value goes straight into an `<audio>` element.

One thing to watch: a chat page served over **https cannot play an http
stream** — browsers block it outright. The page says so rather than failing
silently, but the real fix is TLS on the stream too.

## Behind a reverse proxy

Run it behind nginx/Caddy for TLS. The one thing that needs saying is
`--trust-proxy`:

```sh
./pubsplash-chat --bind 127.0.0.1:8080 --trust-proxy --brand "Night Owl Radio"
```

Without it the spam guard counts every message against the *proxy's* address, so
one flooder throttles the whole room. With it — and **only** behind a proxy that
actually sets `X-Forwarded-For` — the real client address is used. Turning it on
for a directly-exposed server is worse than leaving it off: anybody could then
invent an address per message and never be throttled at all.

nginx needs buffering off, or the feed arrives in lumps:

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_buffering off;           # SSE: deliver events as they happen
    proxy_read_timeout 1h;         # the feed is a long-lived response
}
```

Caddy needs none of that — `reverse_proxy 127.0.0.1:8080` is enough, and it sets
`X-Forwarded-For` itself.

### systemd

```ini
[Unit]
Description=Pubsplash Chat
After=network.target

[Service]
Type=simple
User=pubsplash
WorkingDirectory=/var/lib/pubsplash-chat
ExecStart=/usr/local/bin/pubsplash-chat --bind 127.0.0.1:8080 --trust-proxy --brand "Night Owl Radio"
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
```

The working directory is where the secret file lives, so it must be writable and
must persist — a new secret means every host key you handed out stops working.

That file is the host identity of **every room on the deployment**: anyone who
can read it can derive every host key and post as any broadcaster. It is created
`0600`, and the server warns on startup if it finds one that anybody else can
read (a deployment made before that was added). Back it up somewhere equally
private.

## Options

```
--bind ADDR[:PORT]    where to listen (default 0.0.0.0:8080)
--brand NAME          name shown on the page (default "Chat")
--secret-file PATH    where the secret lives (default ./pubsplash-chat.secret)
--secret VALUE        the secret inline, or set PUBSPLASH_CHAT_SECRET
--trust-proxy         read the client address from X-Forwarded-For
--rate N              accepted messages per window, per sender (default 5)
--rate-window SECONDS the window they are counted over (default 10)
--lang CODE           fallback page language, en or es (default en)
```

### Language

The page speaks English or Spanish, and picks by this order:

1. `?lang=es` on the URL, remembered for that browser afterwards. This is the
   link to hand out for a Spanish-language show whose listeners run an
   English-language browser.
2. Whatever that browser was last told.
3. The browser's own preferred languages — the right default, since nobody
   configures a page they opened from a link.
4. `--lang`, for a station whose audience does not have their browsers set the
   way that assumes.

`<html lang>` follows the choice, which is what decides the voice a screen
reader reads the page with.

## What stops trouble

No registration means no accounts to ban, so the defences are the ones that work
without an identity system:

**Flooding** is a sliding window: five messages per ten seconds per person, and a
**refused message does not count** — hammering the endpoint cannot lock anybody
out, it just does nothing. Above that sits a second, looser ceiling on the whole
IP address, which is what a script regenerating its identity actually runs into.

**Impersonation** is a name claim. The first person to speak as `Alice` in a room
holds that name; anybody else using it is refused, including `alice` and `ALICE`.
The holder can change their own name, but only every five minutes — without that
cooldown the nickname is a free-text field on every message, and one person can
answer themselves as three people or borrow whoever spoke last. The **host key
overrides both**: a troll who claims the broadcaster's name before the show does
not get to keep it.

A name is held against the address *plus* a token the browser generates, so a
household, an office or a mobile carrier's shared address is several people who
can each hold their own name. The **spam budget is counted against the address
alone**, which is the part a client cannot regenerate — so clearing that token
buys a new name and not one extra message.

**Message bodies** are trimmed, capped at 2000 characters (counted in
characters, not bytes), stripped of control characters, and have runs of
whitespace and blank lines collapsed — the usual ways an "empty" message or a
screenful of nothing gets through.

None of this stops a determined person with a VPN. It stops the things that
actually happen to a small station's chat room.

## Accessibility

This is the part the design is actually for; it is a port of
[SonicRoom](../../sonicroom)'s in-room chat, which was built and tested with
screen-reader users.

- **The message list is a listbox you arrow through**, not a log you scroll.
  Up/Down move, Home/End jump, and Ctrl+C copies the message you are on. A blind
  listener can go back over what was said without losing their place, and the
  active row is tracked with `aria-activedescendant` so the list can keep growing
  underneath them.
- **The list comes before the composer**, so tabbing in lands on history first.
  It is always present and always focusable, including in a room where nobody has
  spoken yet.
- **New messages are announced** on a polite live region, and the announcement is
  built from the *same string* as the visible row, so the two can never drift
  apart. The region holds exactly one line and is marked `aria-atomic="false"`,
  because `role="status"` is implicitly atomic — a region that accumulated
  announcements got re-read in full on every change.
- **Alt+1 to Alt+0 read the last ten messages back** from anywhere on the page,
  including while typing. 1 is the newest. Pressing the same digit twice quickly
  copies that message.
- **Two audio cues**: a chime for an arriving message, a dull thunk for a send the
  spam guard refused. Synthesised in the page, so they cost no request.
- **The radio is two plain controls**, a button and a range input, rather than the
  browser's native player chrome — both are easier to reach and to read out, and
  a live stream has no seek bar worth having. Stopping drops the source rather
  than pausing it, so resuming joins the broadcast where it is now instead of
  playing however many minutes had piled up.
- Times are relative and refresh in place; the row you are reading is deliberately
  left alone, so nothing re-announces itself under you.
- Light and dark, `prefers-contrast`, `prefers-reduced-motion`, and usable down
  to a 400px phone.

## The wire protocol

Both clients — the page and Pubsplash's `net::pubchat` — speak this. It is SSE
down and `POST` up, with no WebSocket, so it survives every proxy and reuses the
SSE parser Pubsplash already had for Audio Pub.

`GET /r/{room}/events` replays history and then follows the room:

```
event: hello
data: {"room":"nightowl","brand":"Night Owl Radio","listeners":3,
       "maxChars":2000,"limit":{"messages":5,"windowMs":10000},"history":[…]}

event: chat
data: {"id":"1789104591938-1","nick":"Alice","text":"hi","ts":1789104591938,"host":false}

event: listeners
data: {"count":4}

event: lagged
data: {"missed":12}
```

```
event: stream
data: {"stream":{"url":"https://radio.example.com/live.mp3","name":"Night Owl"}}
```

`POST /r/{room}/messages` with `{"nick":"Alice","text":"hi","client":"<token>"}`
answers `200 {"ok":true,"message":{…}}`, or one of:

| Status | `error` | Meaning |
| --- | --- | --- |
| 400 | `empty` / `too_long` / `bad_room` | Nothing to send, too long, no such room |
| 403 | `bad_host_key` | An `Authorization: Bearer` key that is not this room's |
| 409 | `name_taken` | Somebody else holds that nickname here |
| 409 | `name_locked` | Your own name is still in its cooldown (`retryAfterMs`) |
| 429 | `rate_limited` | Slow down (`retryAfterMs`) |

Send `Authorization: Bearer <host key>` to post as the broadcaster.

`PUT /r/{room}/stream` with `{"url":"…","name":"…"}` publishes the play button.
Host key required — it is the one piece of room state a listener must not be
able to write, since it decides what every page in the room points an `<audio>`
element at.

## Layout

| File | What it is |
| --- | --- |
| `src/main.rs` | CLI, routes, handlers, the two identity keys |
| `src/room.rs` | Rooms: history ring, fan-out, listener count, name claims |
| `src/limit.rs` | The sliding-window spam guard |
| `src/message.rs` | The wire format and every validation rule |
| `src/hostkey.rs` | `HMAC(secret, room)`, and the constant-time check |
| `assets/chat.html` | The listener page, compiled in |
| `assets/index.html` | The room picker, compiled in |

This is its own cargo workspace on purpose: building Pubsplash at the repo root
does not build this, and axum never enters the desktop app's dependency graph.

## Licence

MIT, same as Pubsplash.
