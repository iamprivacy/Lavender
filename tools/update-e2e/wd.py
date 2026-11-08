#!/usr/bin/env python3
"""Tiny W3C WebDriver client for the test app (tauri-plugin-webdriver on :4445).

  wd.py new                      start a session (id kept in session.txt)
  wd.py js 'return ...'          run a synchronous script, print the result
  wd.py invoke CMD [JSON_ARGS]   call a Tauri command, print {ok|err}
  wd.py listen EVENT             record EVENT payloads in window.__events
  wd.py events                   print the recorded events
  wd.py shot PATH                save a PNG screenshot
  wd.py click CSS                click the first element matching CSS
  wd.py text                     print document.body.innerText
"""

import base64
import json
import os
import sys
import urllib.request

E = os.environ.get("WORK", "/private/tmp/lavender-update-e2e")
BASE = "http://127.0.0.1:4445"


def call(method, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(BASE + path, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=120) as r:
        return json.loads(r.read() or b"{}")


def sid():
    with open(os.path.join(E, "session.txt")) as f:
        return f.read().strip()


def js(script, args=None, asynchronous=False):
    kind = "async" if asynchronous else "sync"
    return call("POST", f"/session/{sid()}/execute/{kind}",
                {"script": script, "args": args or []})["value"]


INVOKE = """
const [cmd, args] = [arguments[0], arguments[1]];
const done = arguments[arguments.length - 1];
window.__TAURI_INTERNALS__.invoke(cmd, args)
  .then(r => done({ ok: r === undefined ? null : r }))
  .catch(e => done({ err: String(e) }));
"""

LISTEN = """
const name = arguments[0];
const done = arguments[arguments.length - 1];
window.__events = window.__events || [];
const t = window.__TAURI_INTERNALS__;
const handler = t.transformCallback(e => window.__events.push(e.payload));
t.invoke('plugin:event|listen', { event: name, target: { kind: 'Any' }, handler })
  .then(() => done('listening'), e => done(String(e)));
"""


def main(a):
    if a[1] == "new":
        r = call("POST", "/session", {"capabilities": {}})
        s = r["value"]["sessionId"]
        open(os.path.join(E, "session.txt"), "w").write(s)
        print(s)
    elif a[1] == "js":
        print(json.dumps(js(a[2]), indent=1))
    elif a[1] == "invoke":
        args = json.loads(a[3]) if len(a) > 3 else {}
        print(json.dumps(js(INVOKE, [a[2], args], asynchronous=True), indent=1))
    elif a[1] == "listen":
        print(js(LISTEN, [a[2]], asynchronous=True))
    elif a[1] == "events":
        print(json.dumps(js("return window.__events || []"), indent=1))
    elif a[1] == "shot":
        png = call("GET", f"/session/{sid()}/screenshot")["value"]
        open(a[2], "wb").write(base64.b64decode(png))
        print(a[2])
    elif a[1] == "click":
        el = call("POST", f"/session/{sid()}/element",
                  {"using": "css selector", "value": a[2]})["value"]
        eid = list(el.values())[0]
        call("POST", f"/session/{sid()}/element/{eid}/click", {})
        print("clicked")
    elif a[1] == "text":
        print(js("return document.body.innerText"))


main(sys.argv)
