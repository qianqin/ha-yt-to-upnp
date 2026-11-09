# Built by the Supervisor from the folder that scripts/build-ha-app.sh assembles:
# everything under artefacts/ is prebuilt on the release machine, nothing is
# compiled here. Supervisor 2026.04+ passes no BUILD_FROM, so the pinned base
# image is the default (Debian bookworm: glibc for the receiver and Deno).
ARG BUILD_FROM=ghcr.io/home-assistant/base-debian:bookworm-2026.10.3@sha256:ebb912a11616e3eda2d31204803587276e70bbb4e7f02a6c92dba5f43256b48d
# hadolint ignore=DL3006
FROM ${BUILD_FROM}

ARG BUILD_VERSION=dev
ARG BUILD_ARCH=amd64

# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends mpd python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin ytcast \
    && useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin adapter \
    && useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin potoken \
    && id mpd

COPY artefacts/ytcast-lite artefacts/deno /usr/local/bin/
COPY artefacts/adapter/ /opt/ytcast-adapter/
COPY artefacts/bgutil/ /opt/bgutil/
COPY artefacts/BUILD-INFO /opt/BUILD-INFO
COPY rootfs/ /

# Execute bits do not survive every way of copying the folder onto the HA host.
RUN chmod 0755 /usr/local/bin/ytcast-lite /usr/local/bin/deno /etc/s6-overlay/scripts/*.sh \
        /etc/s6-overlay/s6-rc.d/*/run /etc/s6-overlay/s6-rc.d/*/finish \
    && chmod -R a+rX,go-w /opt/ytcast-adapter /opt/bgutil /usr/lib/ytcast /etc/ytcast

LABEL \
    io.hass.version="${BUILD_VERSION}" \
    io.hass.type="app" \
    io.hass.arch="${BUILD_ARCH}"
