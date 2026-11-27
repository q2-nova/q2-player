#!/usr/bin/env python3
"""Get a Last.fm session key for q2nd and write the card's [lastfm] stanza.

  tools/lastfm_authorize.py --out NEW_FILE --contact-url HTTPS_URL

Run it on a computer with the API key and shared secret of a Last.fm API
account you control (https://www.last.fm/api/account/create). It asks for
both, prints a one-time authorization link to approve in a browser, then
writes a new private file (mode 0600) with `[lastfm]` `enabled`, `api_key`,
`shared_secret`, `session` and `contact_url`. Merge it into the card's
`Q2Mainline/config.toml`. The contact URL is the HTTPS page that identifies
whoever runs the application; q2nd sends it in its User-Agent.
"""

import argparse
import getpass
import hashlib
import json
import os
import urllib.parse
import urllib.request

API = "https://ws.audioscrobbler.com/2.0/"
AUTHORIZE = "https://www.last.fm/api/auth/"
LIMIT = 65536


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


def contact_refusal(url):
    """Why `url` cannot be the application's contact URL, or None."""
    parts = urllib.parse.urlparse(url)
    if (parts.scheme != "https" or not parts.hostname or parts.username or parts.password
            or parts.fragment or parts.query or not url.isascii() or any(c.isspace() for c in url)):
        return "Provide a public HTTPS contact URL without credentials, parameters or a fragment"
    return None


def credential_refusal(key, secret):
    if any(not part or not part.isascii() or not part.isprintable() or any(c.isspace() for c in part)
           for part in (key, secret)):
        return "Enter the API key and shared secret of your Last.fm API account"
    return None


def signed(method, key, secret, **fields):
    """The query of a signed Last.fm API call (api_sig: MD5 of the sorted name-value pairs and the secret)."""
    params = {"api_key": key, "method": method, **fields}
    text = "".join(name + value for name, value in sorted(params.items())) + secret
    params["api_sig"] = hashlib.md5(text.encode(), usedforsecurity=False).hexdigest()
    params["format"] = "json"
    return urllib.parse.urlencode(params)


def call(opener, contact, method, key, secret, **fields):
    request = urllib.request.Request(f"{API}?{signed(method, key, secret, **fields)}",
                                     headers={"User-Agent": f"q2nd setup ({contact})"})
    try:
        with opener.open(request, timeout=15) as response:
            data = response.read(LIMIT + 1)
        if len(data) > LIMIT:
            raise ValueError
        item = json.loads(data)
        if not isinstance(item, dict) or "error" in item:
            raise ValueError
        return item
    except (OSError, ValueError):
        raise SystemExit("Last.fm did not authorize this request. Check the API key and secret and try again.") from None


def stanza(key, secret, session, contact):
    lines = ["[lastfm]", "enabled = true"]
    lines += [f"{name} = {json.dumps(value, ensure_ascii=False)}" for name, value in
              (("api_key", key), ("shared_secret", secret), ("session", session), ("contact_url", contact))]
    return "\n".join(lines) + "\n"


def authorize(opener, contact, key, secret, confirm):
    token = call(opener, contact, "auth.getToken", key, secret).get("token")
    if not isinstance(token, str) or not token:
        raise SystemExit("Last.fm returned no authorization token.")
    confirm(f"{AUTHORIZE}?{urllib.parse.urlencode({'api_key': key, 'token': token})}")
    session = call(opener, contact, "auth.getSession", key, secret, token=token).get("session")
    if not isinstance(session, dict) or not isinstance(session.get("key"), str) or not session["key"]:
        raise SystemExit("Last.fm returned no authorized session.")
    return session["key"]


def confirm_in_browser(link):
    print("Open this one-time authorization link in a browser and approve access:")
    print(link)
    input("Press Enter once Last.fm confirms the authorization. ")


def main(argv=None, opener=None, ask=getpass.getpass, confirm=confirm_in_browser):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", required=True, help="new private file for the [lastfm] stanza")
    parser.add_argument("--contact-url", required=True, help="HTTPS contact page of whoever runs the application")
    args = parser.parse_args(argv)
    refusal = contact_refusal(args.contact_url)
    if refusal:
        parser.error(refusal)
    if os.path.lexists(args.out):
        parser.error("The output file already exists")
    key = ask("Last.fm API key: ").strip()
    secret = ask("Last.fm API shared secret: ").strip()
    refusal = credential_refusal(key, secret)
    if refusal:
        parser.error(refusal)
    session = authorize(opener or urllib.request.build_opener(NoRedirect()), args.contact_url, key, secret, confirm)
    try:
        fd = os.open(args.out, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as out:
            out.write(stanza(key, secret, session, args.contact_url))
    except OSError:
        raise SystemExit("Could not save the authorization output.") from None
    print("Saved the private [lastfm] stanza. Merge it into the card's Q2Mainline/config.toml.")


if __name__ == "__main__":
    main()
