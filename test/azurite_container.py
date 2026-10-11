#!/usr/bin/env python3
"""Create a blob container in Azurite, for tests/gcs_azblob.rs.

Usage: azurite_container.py [endpoint] [container]
Default: http://127.0.0.1:10000/devstoreaccount1 kubedal-test

Azurite needs a signed request to create a container, so this signs it with
the well-known Azurite development account key. An existing container is fine.
"""

import base64
import hashlib
import hmac
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from email.utils import formatdate

ACCOUNT = "devstoreaccount1"
KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="

endpoint = sys.argv[1] if len(sys.argv) > 1 else f"http://127.0.0.1:10000/{ACCOUNT}"
container = sys.argv[2] if len(sys.argv) > 2 else "kubedal-test"
url = f"{endpoint}/{container}?restype=container"
path = urllib.parse.urlparse(url).path

for attempt in range(30):
    date = formatdate(usegmt=True)
    headers = {"x-ms-date": date, "x-ms-version": "2021-08-06"}
    canonical_headers = "".join(f"{k}:{v}\n" for k, v in sorted(headers.items()))
    # Verb, eleven empty standard headers, then the x-ms headers and the resource.
    to_sign = "PUT\n" + "\n" * 11 + canonical_headers + f"/{ACCOUNT}{path}\nrestype:container"
    signature = base64.b64encode(
        hmac.new(base64.b64decode(KEY), to_sign.encode(), hashlib.sha256).digest()
    ).decode()
    headers["Authorization"] = f"SharedKey {ACCOUNT}:{signature}"
    request = urllib.request.Request(url, method="PUT", headers=headers)
    try:
        urllib.request.urlopen(request)
        print(f"created container {container}")
        sys.exit(0)
    except urllib.error.HTTPError as err:
        if err.code == 409:
            print(f"container {container} exists")
            sys.exit(0)
        sys.exit(f"cannot create container {container}: {err.code} {err.read().decode()}")
    except urllib.error.URLError as err:
        # Azurite is not up yet.
        print(f"waiting for Azurite: {err.reason}")
        time.sleep(2)
sys.exit("Azurite did not start")
