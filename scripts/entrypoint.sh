#!/bin/sh
# Auto-generate a self-signed cert if one is not already present.
# In production, mount real certs at /certs and they will be used as-is.
CERT="${CERT_PATH:-/certs/cert.pem}"
KEY="${KEY_PATH:-/certs/key.pem}"

if [ ! -f "$CERT" ] || [ ! -f "$KEY" ]; then
    mkdir -p "$(dirname "$CERT")"
    openssl req -x509 -newkey rsa:2048 -nodes \
        -keyout "$KEY" -out "$CERT" \
        -days 3650 -subj "/CN=mindex" 2>/dev/null
fi

exec mindex "$@"
