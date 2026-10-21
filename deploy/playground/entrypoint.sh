#!/bin/sh
# Starts the designer wallet on loopback and the customer wallet with the public
# playground. Wallets are created on first start. ZAW_PASSWORD and PUBLIC_HOST come
# from the environment.
set -eu

: "${ZAW_PASSWORD:?set ZAW_PASSWORD}"
: "${PUBLIC_HOST:?set PUBLIC_HOST}"

for w in customer designer; do
    if [ ! -f "/data/$w/config.json" ]; then
        zaw --network testnet --data-dir "/data/$w" dev init
    fi
done
mkdir -p /data/customer/inbox

zaw --data-dir /data/designer daemon --listen 127.0.0.1:7342 --unlock-from-env &

exec zaw --data-dir /data/customer daemon \
    --listen 0.0.0.0:7341 \
    --public-host "$PUBLIC_HOST" \
    --unlock-from-env \
    --playground-peer-url http://127.0.0.1:7342 \
    --playground-peer-dir /data/designer
