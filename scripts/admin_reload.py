"""Request a strategy reload over loopback without exposing the token in argv."""

import http.client
import os
import sys


def main():
    token = os.environ.get("PG_ADMIN_TOKEN", "")
    if not 32 <= len(token) <= 512 or any(not 33 <= ord(c) <= 126 for c in token):
        print("Reload not requested: supply a valid PG_ADMIN_TOKEN through "
              "the protected process environment.", file=sys.stderr)
        return 1
    try:
        port = int(os.environ.get("PG_HEALTH_PORT", "8080"))
        if not 1 <= port <= 65535:
            raise ValueError
    except ValueError:
        print("Reload not requested: PG_HEALTH_PORT must be 1-65535.", file=sys.stderr)
        return 1

    # No proxy, redirects, shell, or subprocess. Never print response bodies or
    # exception text: an unexpected server response might echo the credential.
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        connection.request("POST", "/admin/reload",
                           headers={"Authorization": "Bearer " + token})
        status = connection.getresponse().status
    except (OSError, http.client.HTTPException):
        print("Reload request failed: connection or HTTP protocol error.", file=sys.stderr)
        return 1
    finally:
        connection.close()
    if status != 202:
        print(f"Reload not accepted: HTTP {status}.", file=sys.stderr)
        return 1
    print("Reload request accepted (HTTP 202); verify completion in daemon status.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
