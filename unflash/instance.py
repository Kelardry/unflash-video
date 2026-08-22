"""Per-account server instances.

Every account signed in to a PC shares 127.0.0.1, so a server one account
starts is reachable by all of them -- and because the native file dialogs are
spawned by the server process, they open on the desktop of whoever *started*
the server, not whoever clicked the button.

So each server:

  * claims a port nobody else is listening on (8765, then upwards), and
  * hands out a token kept in the starting account's own profile directory;
    requests without it are refused.

A second copy started by the same account finds that account's live server
through the recorded port + token and just re-opens the browser on it.
"""

import json
import os
import secrets
import socket
import urllib.error
import urllib.request

DEFAULT_PORT = 8765
PORT_SPAN = 64
TOKEN_BYTES = 24


# --- per-account state file --------------------------------------------------

def state_dir():
    """A directory only this account can write (and, on Windows, read)."""
    if os.name == "nt":
        base = os.environ.get("LOCALAPPDATA") or os.path.expanduser("~")
        d = os.path.join(base, "Unflash")
    else:
        base = (os.environ.get("XDG_STATE_HOME")
                or os.path.join(os.path.expanduser("~"), ".local", "state"))
        d = os.path.join(base, "unflash")
    os.makedirs(d, exist_ok=True)
    return d


def state_file():
    return os.path.join(state_dir(), "instance.json")


def load_state():
    try:
        with open(state_file(), "r", encoding="utf-8") as f:
            d = json.load(f)
        return d if isinstance(d, dict) else {}
    except (OSError, json.JSONDecodeError):
        return {}


def save_state(**kw):
    d = load_state()
    d.update(kw)
    tmp = state_file() + ".tmp"
    try:
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(d, f)
        os.replace(tmp, state_file())
        if os.name != "nt":
            os.chmod(state_file(), 0o600)
    except OSError:
        pass
    return d


def user_token():
    """This account's token, stable across restarts so an already-open tab
    (or a bookmark) keeps working after the server is restarted."""
    tok = load_state().get("token")
    if not isinstance(tok, str) or len(tok) < 16:
        tok = secrets.token_urlsafe(TOKEN_BYTES)
        save_state(token=tok)
    return tok


# --- ports -------------------------------------------------------------------

def is_listening(host, port, timeout=0.4):
    try:
        with socket.create_connection((host, port), timeout):
            return True
    except OSError:
        return False


def is_free(host, port):
    """True if we could bind this port right now.

    Windows lets a second socket bind a port that already has a listener when
    SO_REUSEADDR is set (which is the default for Python's HTTP servers), and
    then splits connections between them unpredictably; SO_EXCLUSIVEADDRUSE
    makes the test honest.
    """
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        if hasattr(socket, "SO_EXCLUSIVEADDRUSE"):
            s.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        s.bind((host, port))
        return True
    except OSError:
        return False
    finally:
        s.close()


def candidate_ports(host, base=DEFAULT_PORT, span=PORT_SPAN):
    """Ports from ``base`` upwards that nothing is holding, best first."""
    for p in range(base, base + span):
        if is_free(host, p) and not is_listening(host, p):
            yield p


# --- finding our own running server ------------------------------------------

def probe(host, port, token, timeout=1.0):
    """Ask whoever is on this port whether they are our own instance.

    A server belonging to another account answers 403 (no token match), a
    non-Unflash server answers something else; both read as "not ours".
    """
    req = urllib.request.Request(f"http://{host}:{port}/api/instance",
                                 headers={"X-Unflash-Token": token})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            data = json.loads(r.read().decode("utf-8"))
    except (urllib.error.URLError, OSError, ValueError):
        return None
    return data if isinstance(data, dict) and data.get("unflash") else None


def post(host, port, token, path, payload, timeout=60.0):
    """Call our own running server. Returns None on success, else a message."""
    req = urllib.request.Request(
        f"http://{host}:{port}{path}",
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json",
                 "X-Unflash-Token": token})
    try:
        urllib.request.urlopen(req, timeout=timeout).close()
        return None
    except urllib.error.HTTPError as e:
        try:
            return json.loads(e.read().decode("utf-8")).get("error") or str(e)
        except (ValueError, OSError):
            return str(e)
    except (urllib.error.URLError, OSError) as e:
        return str(e)


def find_own(host, token):
    """The port of this account's already-running server, or None."""
    d = load_state()
    port = d.get("port")
    if not isinstance(port, int):
        return None
    if d.get("host") not in (None, host):
        return None
    return port if probe(host, port, token) else None
