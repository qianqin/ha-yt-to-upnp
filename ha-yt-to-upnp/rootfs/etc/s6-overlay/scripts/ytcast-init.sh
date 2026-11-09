#!/command/with-contenv bashio
# shellcheck shell=bash
# Runs once before the services. A failure here stops the app: the base image
# sets S6_BEHAVIOUR_IF_STAGE2_FAILS=2, so a failed oneshot halts the container
# instead of starting (and restarting) services on bad options.
set -e

# with-contenv keeps the container's environment, the Supervisor token among it,
# in files here, world-readable by default. The services drop to their own users
# right away; they must not be able to read it.
chmod 0700 /run/s6/container_environment

if ! problem="$(python3 -I /usr/lib/ytcast/check_options.py /data/options.json)"; then
    # bashio logs with echo -e: double the backslashes so that a rule like
    # \x00-\x1f is shown as written, not turned into control characters.
    bashio::exit.nok "${problem//\\/\\\\}; fix the app's configuration and start it again"
fi

# Persistent state under /data, private to the user of each service.
mkdir -p /data/ytcast /data/mpd/music /data/mpd/playlists
chown -R ytcast:ytcast /data/ytcast
chown -R mpd: /data/mpd
chmod 0700 /data/ytcast /data/mpd
