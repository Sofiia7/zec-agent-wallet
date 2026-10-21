# Testnet playground: two zaw wallets (customer and designer) in one container.
# Build context: this directory plus a Linux `zaw` binary built by CI.
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY zaw /usr/local/bin/zaw
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 755 /usr/local/bin/zaw /usr/local/bin/entrypoint.sh \
    && useradd --system --home /data zaw && mkdir -p /data && chown zaw /data
USER zaw
VOLUME /data
EXPOSE 7341
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
