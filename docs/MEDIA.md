# Media page: lyrics (opt-in)

The Media page itself needs no setup: it follows whatever Windows considers the current media
session. This note is about the one option that talks to the internet.

## Lyrics (`[media] lyrics = true`, **off by default**)

A line under the transport controls shows the lyric being sung, following the playback position.

* **What is sent, and where.** While the Media page is open, once per track: the artist, title, album
  and length of the track, in the URL of an HTTPS request to `lrclib.net`, with the user agent
  `SharkNotch/<version>`. Nothing else, no account, no identifier. At most two requests per track
  (the exact lookup, then the looser search if LRCLIB has no record under those words), one after the
  other, never in parallel.
* **What is kept.** The lyrics of the current track, in memory. Nothing is written to disk.
* **What is shown.** Only *timed* lyrics. A track whose lyrics have no timing says "No timed lyrics";
  instrumentals say so; a failed request shows nothing and is tried again on the next track.
* **Accuracy.** The line is picked from the player's position, which the page re-reads every few
  seconds, and it redraws once a second: it can lag a beat.

### Terms: not verified

[LRCLIB](https://lrclib.net) describes itself as "a completely free service for finding and
contributing synchronized lyrics", and its server code is MIT-licensed. When this was written, **no
terms of use, licence for the lyrics themselves, rate limits or attribution rules could be found**
(its GitHub page does not state them and the site's documentation could not be read). Lyrics are
usually copyrighted by their writers or publishers, and an MIT licence on the code does not say what
holds for the text. That is why the option is off by default, why nothing is stored, and why this page
exists: turning it on is your decision, and the risk of displaying lyrics is yours. If LRCLIB
publishes terms that forbid this use, delete `services/lyrics.rs` and the option.

### Checked

* Parsing of timed lyrics, picking the current line, request building and the module's behaviour
  (off by default, one request per track, a late answer for another track ignored): unit tests.
* One real lookup against `lrclib.net` (a well-known track, and one that does not exist) from a
  Windows machine: `cargo test -p shark-notch -- --ignored lrclib` (needs the network).
* Not checked: other players' timing accuracy, tracks whose metadata differs from LRCLIB's records.
