#!/usr/bin/env python3
"""Bounded, redacted subprocess failure diagnostics shared by the packaging
scripts, so a `docker`/`cargo` failure under `scripts/build.py` is
diagnosable from its final error message instead of only "apsides: image
build failed" with no detail.

Every packaging script's `if __name__ == "__main__":` block catches a broad
`Exception` and re-raises a short `SystemExit("apsides: <label> failed")`,
deliberately discarding whatever the caught exception carried, so a routine
usage error (a missing file, an invalid argument) never leaks an internal
path or traceback. Two exception types are meant to carry detail past that
boundary, and `failure_message()` is the one place that adds it back,
redacted:

- `SubprocessFailure` for a `docker`/`cargo`/etc. subprocess that exited
  non-zero: its captured output, bounded to the last `DEFAULT_TAIL_LINES`
  lines.
- `ActionableError` for a prepare-time check whose message already tells
  the user what to do about it -- which environment variable to set, which
  file to edit -- so it must never be swallowed by the generic
  "apsides: ... failed" fallback the way a routine `ValueError` is. Raise
  it (it is a `ValueError`, so existing `except ValueError`/
  `assertRaisesRegex(ValueError, ...)` call sites keep working) for
  anything a person reading only the final line needs to act on.
"""
import re

REDACTED = "[REDACTED]"
DEFAULT_TAIL_LINES = 200

# Text shapes that look like a token, key or credential, applied to
# subprocess output before it is ever included in a build failure message.
# Each pair is (pattern, replacement); a replacement referencing `\1` keeps
# a matched label (e.g. "Authorization: Bearer ") and redacts only the
# secret value after it. This is a conservative, bounded set of recognisable
# credential shapes -- bearer/API-key headers, basic-auth URLs, common
# provider token prefixes, signed-URL query parameters, and
# key/token/secret/password assignments -- not a complete secret scanner.
_RULES = [
    (re.compile(r"(?i)(authorization:\s*bearer\s+)\S+"), r"\1" + REDACTED),
    (re.compile(r"(?i)(x-[a-z0-9-]*(?:api-?key|token)\s*:\s*)\S+"), r"\1" + REDACTED),
    (re.compile(r"://[^/\s:@]+:[^/\s:@]+@"), f"://{REDACTED}@"),
    (re.compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b"), REDACTED),
    (re.compile(r"\bAKIA[0-9A-Z]{16}\b"), REDACTED),
    (re.compile(r"\bey[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b"), REDACTED),
    (
        re.compile(
            r"(?i)([?&](?:sig|signature|token|jwt|sas|x-amz-signature|x-amz-credential|"
            r"x-amz-security-token|access_token|api_key|apikey)=)[^&\s'\"]+"
        ),
        r"\1" + REDACTED,
    ),
    (
        re.compile(
            r"(?i)\b((?:api|access|secret|private)[-_]?key|token|password|passwd|secret|credential)(s?\s*[:=]\s*)\S+"
        ),
        r"\1\2" + REDACTED,
    ),
]


def redact(text):
    for pattern, replacement in _RULES:
        text = pattern.sub(replacement, text)
    return text


def bounded(text, lines=DEFAULT_TAIL_LINES):
    parts = text.splitlines()
    return text if len(parts) <= lines else "\n".join(parts[-lines:])


def diagnostic(text, lines=DEFAULT_TAIL_LINES):
    """Bounded, redacted diagnostic text safe to put in a failure message."""
    return bounded(redact(text or ""), lines)


class SubprocessFailure(RuntimeError):
    """A subprocess (docker/cargo/etc.) exited non-zero.

    `output` is its captured, unredacted, unbounded stderr (or combined
    stdout+stderr, for a step where they are merged) -- reserved for
    `failure_message()`, which is the only place it should be bounded and
    redacted before being shown.
    """

    def __init__(self, summary, output):
        super().__init__(summary)
        self.summary = summary
        self.output = output or ""


class ActionableError(ValueError):
    """A prepare-time failure whose message already tells the user the
    remedy: which environment variable to set, which file to edit, which
    value to change. `failure_message()` always surfaces this message
    (redacted, never bounded away) -- unlike a plain `ValueError` from a
    routine usage mistake (a missing file, a bad argument), which the
    top-level handlers deliberately discard so they never leak an internal
    path or traceback.

    A subclass of `ValueError`, so existing `except ValueError` and
    `assertRaisesRegex(ValueError, ...)` call sites keep matching it.
    """


def failure_message(label, error):
    """The final `apsides: <label> failed` message for a caught top-level
    `error`: a bounded, redacted subprocess diagnostic appended when `error`
    is a `SubprocessFailure` that actually captured output, or its own
    redacted message appended verbatim when it is an `ActionableError` --
    otherwise just the plain label, so a routine usage error never leaks
    detail past this boundary."""
    message = f"apsides: {label} failed"
    if isinstance(error, SubprocessFailure) and error.output.strip():
        detail = diagnostic(error.output)
        if detail:
            message += "\n" + detail
    elif isinstance(error, ActionableError) and str(error).strip():
        message += "\n" + redact(str(error))
    return message
