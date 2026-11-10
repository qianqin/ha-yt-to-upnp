# Built by the Supervisor from this folder, as Home Assistant takes it from the
# repository: nothing is compiled here. The prebuilt programs (receiver,
# adapter, Deno, PoToken helper) come from the GitHub release tarball named in
# artefacts.lock, checked against its sha256; a folder assembled by
# scripts/build-ha-app.sh brings them along in artefacts/ instead. The base
# image is pinned by digest, the same in both stages (Debian bookworm: glibc for
# the receiver and Deno); Supervisor 2026.04+ passes no BUILD_FROM, and it adds
# the io.hass.* labels itself.
FROM ghcr.io/home-assistant/base-debian:bookworm-2026.10.3@sha256:ebb912a11616e3eda2d31204803587276e70bbb4e7f02a6c92dba5f43256b48d AS artefacts
ARG BUILD_VERSION
# Set only by scripts/smoke-ha-app.sh --release-path (see fetch-artefacts.sh).
ARG ARTEFACTS_URL
# artefact[s] matches the artefacts/ folder if the build context has one and
# nothing otherwise; a matched folder's contents land in /stage. Without
# artefacts/, fetch-artefacts.sh needs artefacts.lock (artefacts.loc[k]: a
# folder assembled before the first release has none).
COPY fetch-artefacts.sh artefacts.loc[k] artefact[s] /stage/
RUN sh /stage/fetch-artefacts.sh /stage /artefacts

FROM ghcr.io/home-assistant/base-debian:bookworm-2026.10.3@sha256:ebb912a11616e3eda2d31204803587276e70bbb4e7f02a6c92dba5f43256b48d

# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends mpd python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin ytcast \
    && useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin adapter \
    && useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin potoken \
    && id mpd

# Execute bits do not survive every way of copying the folder onto the HA host,
# hence the chmods. The PoToken helper (Deno, bgutil) changes least, so it comes
# first: scripts/smoke-ha-app.sh --quick keeps everything above the boundary
# line below as a cached image (it builds this stage alone, copying from
# artefacts/ in the build context).
COPY --from=artefacts /artefacts/deno /usr/local/bin/
COPY --from=artefacts /artefacts/bgutil/ /opt/bgutil/
RUN chmod 0755 /usr/local/bin/deno && chmod -R a+rX,go-w /opt/bgutil
# smoke-quick-cache-boundary
COPY --from=artefacts /artefacts/ytcast-lite /usr/local/bin/
COPY --from=artefacts /artefacts/adapter/ /opt/ytcast-adapter/
COPY --from=artefacts /artefacts/BUILD-INFO /opt/BUILD-INFO
COPY rootfs/ /
RUN chmod 0755 /usr/local/bin/ytcast-lite /etc/s6-overlay/scripts/*.sh \
        /etc/s6-overlay/s6-rc.d/*/run /etc/s6-overlay/s6-rc.d/*/finish \
    && chmod -R a+rX,go-w /opt/ytcast-adapter /usr/lib/ytcast /etc/ytcast
