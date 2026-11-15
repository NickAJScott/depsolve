# A real, single-node CINC Server for testing the cookbook end to end. Used by test/run.sh.
#
# The first start runs `cinc-server-ctl reconfigure`, which takes several minutes
# (longer under amd64 emulation). The server is ready when the container log shows
# "cinc-server ready".
#
# Run it with --privileged: reconfigure sets kernel parameters for PostgreSQL and OpenSearch.
FROM --platform=linux/amd64 ubuntu:24.04

ARG CINC_SERVER_VERSION=15.10.127
ARG CINC_SERVER_SHA256=7d0cf7854c7b176700b647d2c0478db718f8889ffa2e2785eb52b6e6b3eeb621

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates curl iproute2 procps tzdata \
 && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL -o /tmp/cinc-server.deb \
      "https://downloads.cinc.sh/files/stable/cinc-server/${CINC_SERVER_VERSION}/ubuntu/24.04/cinc-server-core_${CINC_SERVER_VERSION}-1_amd64.deb" \
 && echo "${CINC_SERVER_SHA256}  /tmp/cinc-server.deb" | sha256sum -c - \
 && dpkg -i /tmp/cinc-server.deb \
 && rm /tmp/cinc-server.deb

# cinc-server-ctl fails to load its plugins without a UTF-8 locale, and the database
# migrations (sqitch) fail without a time zone.
ENV LANG=C.UTF-8 LC_ALL=C.UTF-8 TZ=UTC

# No systemd in a container; see systemctl-shim.
COPY systemctl-shim /usr/bin/systemctl
RUN chmod +x /usr/bin/systemctl

COPY start.sh /usr/local/bin/start-cinc-server
RUN chmod +x /usr/local/bin/start-cinc-server

EXPOSE 443
CMD ["/usr/local/bin/start-cinc-server"]
