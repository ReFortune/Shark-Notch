# iPhone link

Your iPhone can send things to the notch: **text and links** (they land in the clipboard history),
**files and photos** (they land on the shelf), its **battery** and its **Focus** (the page shows
them, and an active Focus puts a small chip on the pill), and a **banner** of your own making.
There is no app on the phone: iOS **Shortcuts** does the sending, with plain web requests to a
small server the PC runs on your own network.

Everything on the notch treats a phone item exactly like a PC item (same events, same pages); they
carry a small "iPhone" label so you can tell.

> **Honest scope.** The server, the request rules and the security checks are tested in CI
> (including a client that talks to the real listener over loopback and tries the things listed in
> [What is checked](#what-is-checked)). **No real iPhone was available to the build:** the
> Shortcuts steps below are written from how the app works, and the names of buttons and menus can
> differ a little between iOS versions. If a step does not match your phone, the *requests* in the
> [reference](#reference) are what matter; Shortcuts is only a way to send them.

## Switch it on (on the PC)

The link is **off by default**. A listener is something you switch on, knowing what it is.

1. Open the settings (tray icon → *Open settings (config.toml)*, or the button on the iPhone page) and set:

   ```toml
   [phone]
   listen = true
   ```

   Save. The notch picks it up at once (no restart). The link runs only while the iPhone page
   exists, which it does by default (`[phone] enabled = true` and `"phone"` in `[modules] order`):
   the page is also where you copy the token.

2. Windows Defender Firewall asks whether to let **Shark Notch** talk on the network the first time
   it listens. Tick **Private networks** and **untick Public networks**, then *Allow*. (If you
   clicked *Cancel* or your Wi-Fi is classified *Public*: Settings → Network & internet → Wi-Fi →
   your network → set *Network profile type* to **Private**, and under Windows Security → Firewall
   → *Allow an app through firewall* tick Shark Notch for Private.)

3. Hover the notch and flip to the **iPhone** page. It says **Listening** and shows the address,
   for example `http://192.168.1.20:8765`.

4. Press **Copy token**. The token is now on the PC's clipboard, flagged so that neither the notch's
   clipboard history nor Windows' own clipboard history and cloud sync keep it. Get it to your phone
   by any private way you already have: a note in an app that syncs to the phone, a message to
   yourself, your password manager. Delete it from the message app afterwards if others can read it.

   The token is 32 letters and digits (160 bits of randomness). **New token** (tap twice) makes a
   new one; the old one stops working at once and so does every lockout. You will have to paste the
   new one into your shortcuts.

If your router hands out a different address after a restart, the shortcuts stop finding the PC. Give
the PC a fixed address ("DHCP reservation" in the router) so you set this up once.

## A first shortcut: test the connection

1. In **Shortcuts**, make a new shortcut and add **Get Contents of URL**.
2. URL: `http://192.168.1.20:8765/ping` (use the address from the page).
3. Tap the arrow to show more: **Method** *GET*. **Headers** → *Add new header*: key
   `Authorization`, text `Bearer ` followed by your token (the word *Bearer*, one space, the token).
4. Add **Show Result** after it, and run the shortcut.

The first time, iOS asks whether Shortcuts may find and connect to devices on your local network:
**Allow**. (Later: Settings → Privacy & Security → Local Network → Shortcuts.)

You should see `{"name":"Shark Notch","ok":true,"version":"…"}` and the iPhone page shows
"Last: connection test · just now". Anything else: see [Troubleshooting](#troubleshooting).

Every shortcut below is this same action with another path and, for most, a body. Copy the action
into each shortcut (Shortcuts has no shared secret between shortcuts; if you would rather keep the
address and token in one place, keep them in a file in iCloud Drive and read it with *Get File* and
*Get Dictionary Value*).

## Send the clipboard (text or a link)

1. **Get Clipboard**.
2. **Get Contents of URL**: URL `…/clipboard`, **Method** *POST*, header `Authorization` as above,
   **Request Body** *JSON*, *Add new field* → **Text**, key `text`, value: the **Clipboard**
   variable.

Add the shortcut to the Home Screen, or bind it to Back Tap or the Action Button. On the PC the
notch peeks "Copied on iPhone" and the item sits in the clipboard history, tagged "iPhone". It is **not** put on the
PC's live clipboard: you click it when you want it, so the phone can never replace what you are
about to paste.

For text you select in an app (Share Sheet): in the shortcut's details turn on **Show in Share
Sheet**, accept *Text* and *URLs*, and use **Shortcut Input** as the value of `text`.

## Send files and photos to the shelf

1. In the shortcut's details turn on **Show in Share Sheet** and accept *Images*, *Media*, *Files*
   (and *PDFs* if you like).
2. **Repeat with Each** over **Shortcut Input** (so several selected files all go).
3. Inside, **Get Contents of URL**: URL `…/file`, **Method** *POST*, header `Authorization`, a second
   header `X-File-Name` whose value is the **Repeat Item**'s **Name**, and **Request Body** *File*
   with the **Repeat Item** as the file.

The file arrives on the **shelf** page, labelled from the iPhone, and you can drag it out like any
other. Notes:

* Send the file itself (*File* body). *Form* (multipart) is not understood.
* Without a name the PC calls the file `phone-<date>-<time>` and adds an extension if it can tell
  one from the content type. Photos are HEIC unless you add **Convert Image** (to JPEG) first; the
  PC's own Photos app may not open HEIC without an extension from the Microsoft Store.
* The limit is `max_file_mib` (100 MiB). A bigger file is refused *before* any of it is sent.
* Where it goes, how long it stays, and what is never accepted: see [Files](#files).

## Report the phone's battery

Shortcuts cannot push the battery continuously; iOS lets a shortcut run when something happens. Make
**Personal Automations** (Shortcuts → Automation → *+* → *Personal Automation*), each set to **Run
Immediately** with **Notify When Run** off:

* trigger **Charger** → *Is Connected*; actions: **Get Battery Level**, then **Get Contents of URL**:
  URL `…/battery`, *POST*, header `Authorization`, body *JSON* with **Number** `percent` = the
  **Battery Level** and **Boolean** `charging` = *Yes*;
* trigger **Charger** → *Is Disconnected*: the same with `charging` = *No*;
* optionally trigger **Battery Level** (falls below 20 %, rises above 80 %) and **Time of Day** ones
  with the same actions, to refresh the number during the day.

The iPhone page shows the level with a bar and a bolt while charging. It is "the last thing the
phone said", so it can be stale; the page says when the phone last sent anything.

## Report the phone's Focus

Two **Personal Automations** per Focus you care about, **Run Immediately**:

* trigger **Focus** → pick it (say *Work*) → *When Turning On*; action **Get Contents of URL**: URL
  `…/focus`, *POST*, header `Authorization`, body *JSON* with **Text** `name` = `Work` and **Boolean**
  `active` = *Yes*;
* the same trigger with *When Turning Off*; `active` = *No*.

While it is on, the pill carries a small chip with the Focus name, and the iPhone page shows it.

## A banner from your own shortcut

`/notify` shows a banner (with the other notifications, labelled iPhone) from any shortcut: *POST*,
JSON `title` (required), `body` and `app` (both optional). For example at the end of a long
shortcut, so the PC tells you it finished. **iOS does not hand your phone's notifications to
Shortcuts, so this cannot mirror them**; it only shows what a shortcut chooses to send.

## Reference

Every request: `Authorization: Bearer <token>` in a **header** (never in the address: addresses end
up in logs and histories). One request per connection, `Content-Length` bodies only (chunked
uploads are refused), IPv4 only.

| Request | Body | What happens | Needs |
|---------|------|--------------|-------|
| `GET /ping` | none | `200` with the app's name and version. A test that the address, the network and the token are right. | |
| `POST /clipboard` | JSON `{"text": "…"}` (also `url` or `link` instead of `text`), or a bare JSON string, or plain text with `Content-Type: text/plain` | Added to the clipboard history, from the phone. | clipboard module |
| `POST /file?name=…` | the file's bytes; the name may also come as header `X-File-Name` | Saved in the inbox (below), shown on the shelf, from the phone. `200` with `{"saved": "<name>"}`. | shelf module |
| `POST /battery` | JSON `{"percent": 0–100, "charging": true/false}` (`percent` may be text like `"82%"`; `level` also works) | Shown on the iPhone page. | |
| `POST /focus` | JSON `{"name": "Work", "active": true/false}` | Shown on the page; an active Focus puts a chip on the pill. | |
| `POST /notify` | JSON `{"title": "…", "body": "…", "app": "…"}` | A banner and a row in the notifications list, from the phone. | notifications module |

A request for a module that is switched off on the PC gets `403` and a message that says so.

| Status | Meaning |
|--------|---------|
| `200` | Done. |
| `400` | The request or its JSON is not what the address takes; the message says what it expected. A file that did not arrive in full also answers `400`. |
| `401` | No token, or a wrong one. Also what **every** unknown address answers without a token, so a stranger cannot learn which addresses exist. |
| `403` | The module for this request is switched off on the PC. |
| `404` / `405` | No such address / wrong method for it (after the token was accepted). |
| `411` | A body needs a `Content-Length`. |
| `413` | Larger than allowed (`max_file_mib` for files, 64 KiB for everything else). Refused before the body is read. |
| `415` | Not JSON where JSON is expected, or a file type that is never accepted (below). |
| `429` | Too many wrong tokens from this address: wait the number of seconds in `Retry-After`, or press **New token** on the PC. |
| `431` | The request headers are longer than 8 KiB. |
| `507` | The PC could not keep the file (disk full, or the inbox holds more than it may). |

## Files

* A received file is written to `%LOCALAPPDATA%\SharkNotch\phone-inbox\<time>-<random>\<name>`:
  **a folder per file**, so two files of one name never collide and the sender never overwrites
  anything. The sender chooses a *name* only: folder parts (`..\`, `C:\`) are cut off, characters
  Windows rejects, device names (`CON`, `NUL`…) and text-direction tricks are removed.
* The file carries Windows' **"downloaded from the internet" mark** (a `Zone.Identifier` stream), so
  SmartScreen and Office's Protected View look at it before it can do anything, as for a browser
  download. The notch **never opens or runs** it; the shelf only holds a reference.
* **Programs and scripts are refused** (`415`), by the last extension of the name, whatever case:
  `exe com scr bat cmd msi msp mst ps1 psm1 psd1 ps1xml psc1 vbs vbe vb vbscript js jse wsf wsh ws wsc
  sct hta lnk url reg dll sys ocx drv jar cpl inf scf shb shs appx msix appxbundle msixbundle gadget
  pif application chm msc xll wll vsto diagcab settingcontent-ms library-ms searchconnector-ms theme
  themepack rdp jnlp iso img vhd vhdx py pyw pyz pl rb`. A phone sends photos, documents and
  recordings; there is no good reason to hand the PC something to double-click. (A script you really
  want to send: put it in a zip first.)
* A file that does not arrive in full is deleted; the shelf only ever hears about whole files.
* Received files older than `keep_days` (14) are deleted from the inbox, when the listener starts
  and after each new file. Only folders this app made are ever touched. Files you dragged out of
  the shelf to somewhere else are yours and stay.
* The inbox may hold about 2 GiB (more if you raised `max_file_mib`); beyond that `507`.

## Security

What it is protected by, and what it is not.

* **Off until you switch it on**, and when it is off there is no listener, no thread and no open
  port. While on and idle it is one thread asleep in `accept()` (no timer, nothing polling).
* **Who can connect.** Only private addresses (`10.*`, `172.16–31.*`, `192.168.*`, link-local, and
  the PC itself), and by default (`same_network_only`) only a peer on the **same network as one of
  this PC's own adapters**, so the phone on your Wi-Fi is in and a private network reached through
  a router or VPN is out. A connection from anywhere else is dropped without a byte being read.
  Do **not** forward the port on your router; it would not work, but it is not a thing to try.
* **The token.** 160 random bits from the operating system, compared in constant time, accepted in
  the `Authorization` header only. Stored as plain text in
  `%LOCALAPPDATA%\SharkNotch\phone-token`, protected by your Windows account (like your other
  app data). Five wrong tokens within a minute lock that address out for two minutes.
* **What it checks first.** The order is: who you are, the request head (8 KiB, 10 s), the token,
  what you asked for, the declared size against the limit, and only then the body. A slow or
  stalled upload is cut off; at most 8 requests are served at once and 3 from any one address, so a
  host that opens connections and says nothing cannot shut the phone out.
* **Plain HTTP.** There is **no encryption** on the link: anyone who can read the traffic on your
  network (not just a stranger: someone who knows the Wi-Fi password can often see other devices'
  traffic) can read the token and whatever you send. TLS needs a certificate your phone trusts,
  which is a setup of its own and not something a tiny app can do for you. So: **use it on a network
  you trust (home), not on café, hotel or campus Wi-Fi**, and switch `listen` off when you are away.
  If you ever suspect the token leaked, press **New token**.
* **Another account on the same PC.** Windows lets a program run by *another user* of this PC bind
  the same port more specifically and receive the phone's connections. If you share the PC with
  people you do not trust, leave the link off.
* **While a game is fullscreen** the listener keeps listening (it costs nothing asleep) but the
  notch is hidden: a banner from the phone is queued silently and shown as "missed" afterwards,
  like any other.
* **It never injects into other programs and installs no keyboard or mouse hooks** (nothing in the
  app does); the listener is an ordinary TCP server in the app's own process.

## Troubleshooting

| Symptom | Likely cause |
|---------|--------------|
| "Could not connect to the server" | Phone and PC on different networks (guest Wi-Fi, mobile data, a VPN). The firewall rule (Private) is missing or the network is classified *Public*. The address changed. Test on the PC: `curl.exe -H "Authorization: Bearer TOKEN" http://127.0.0.1:8765/ping` |
| `401` | The token has a stray space or line break (copy it again), or you pressed **New token** since. |
| `429` | Five wrong tokens: wait, or **New token** on the PC. |
| The page says **Not listening**, and a reason | The port is used by another program: change `port` (or use `0`: any free port; the page shows it). |
| Nothing at all arrives, `200` answers | The module is on but the notch is hidden by a fullscreen app: look at the page afterwards. |
| `403` | The matching module is off: `clipboard`, `shelf` or `notifications` under `[…] enabled`, or removed from `[modules] order`. |

## What is checked

CI runs these against the real listener over loopback, on every push:

* no token, and a wrong token, are `401` for known and unknown addresses alike; the right one works;
* clipboard text, battery, Focus and a notification each arrive as events; two files arrive on the
  shelf; the clipboard history holds the text; the Focus chip appears;
* a file's bytes are intact, its folder is named by time and chance, it carries `ZoneId=3`, and a
  name like `..\..\evil.txt` becomes `evil.txt` with nothing written outside the inbox;
* a program (`tool.exe`) is `415`, a 5 MiB promise against a 1 MiB limit is `413` before the body,
  XML is `415`, broken JSON and a missing percentage are `400`, a 9 KB header is `431`;
* five wrong tokens lock the address (`429` with `Retry-After`) even for the right token;
* **Copy token** puts the token on the clipboard and **not** in the clipboard history; **New token**
  kills the old one, works at once and clears the lockout;
* the page shows the address, the battery, the Focus and the counts; with nobody connected the
  listener uses about nothing (the report prints the CPU for a quiet stretch);
* switching `listen` off closes the port and removes the chip.

Not checked, because nothing here could: a real phone and the Shortcuts app, a real Windows
Firewall prompt, a router with client isolation, and Wi-Fi networks of any kind (CI has none).
