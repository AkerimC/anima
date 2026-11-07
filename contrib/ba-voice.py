#!/usr/bin/env python3
"""Download a Blue Archive student's voice lines from bluearchive.wiki.

    contrib/ba-voice.py "Hoshino (Swimsuit)" [dest]
    contrib/ba-voice.py --effects        # shared sound effects (pick up, let go)

Saves the .ogg files to dest (default:
~/.local/share/anima/ba/<Name>/voice/), where animad finds them next to the
model. Lines keep the game's names (Lobby_1, Battle_Victory_1, ...), which
is what animad uses to pick one for each moment.
"""

import concurrent.futures
import html
import os
import re
import sys
import urllib.parse
import urllib.request

UA = {"User-Agent": "anima-voice-fetch/1.0"}


def get(url):
    with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=30) as r:
        return r.read()


# sound effects animad plays for every character (src/voice.rs, Effect)
EFFECTS = ["SE_Cartoon_01.wav", "SE_Cartoon_02.wav"]


def effects():
    dest = os.path.expanduser("~/.local/share/anima/sounds")
    os.makedirs(dest, exist_ok=True)
    for name in EFFECTS:
        text = get("https://bluearchive.wiki/wiki/File:" + name).decode("utf-8", "replace")
        m = re.search(r'//static\.wikitide\.net/bluearchivewiki/[0-9a-f]/[0-9a-f]{2}/' + re.escape(name), text)
        if not m:
            sys.exit(f"{name} not found on the wiki")
        with open(os.path.join(dest, name), "wb") as f:
            f.write(get("https:" + m.group(0)))
    print(f"{len(EFFECTS)} effects in {dest}")


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    if sys.argv[1] == "--effects":
        return effects()
    name = sys.argv[1]
    dest = sys.argv[2] if len(sys.argv) > 2 else os.path.expanduser(f"~/.local/share/anima/ba/{name}/voice")
    page = "https://bluearchive.wiki/wiki/" + urllib.parse.quote(name.replace(" ", "_")) + "/audio"
    text = get(page).decode("utf-8", "replace")
    # original files, not the transcoded mp3 copies
    urls = sorted(set(re.findall(r'src="(//static\.wikitide\.net/bluearchivewiki/[0-9a-f]/[0-9a-f]{2}/[^"/]+\.ogg)"', text)))
    if not urls:
        sys.exit(f"no voice lines found at {page}")
    os.makedirs(dest, exist_ok=True)

    def fetch(u):
        fname = urllib.parse.unquote(html.unescape(u.rsplit("/", 1)[1]))
        path = os.path.join(dest, fname)
        if os.path.exists(path):
            return
        with open(path + ".part", "wb") as f:
            f.write(get("https:" + html.unescape(u)))
        os.replace(path + ".part", path)

    # the wiki's file server is slow per request, not per byte: several at once
    with concurrent.futures.ThreadPoolExecutor(8) as pool:
        list(pool.map(fetch, urls))
    print(f"{len(urls)} lines in {dest}")


if __name__ == "__main__":
    main()
