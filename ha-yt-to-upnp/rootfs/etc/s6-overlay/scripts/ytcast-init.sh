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

# Persistent state under /data, private to the user of each service (the adapter
# also warns about a state directory others can write to). Only the directories
# are set up: the services create their own files in them. A symlink in their
# place would make the chown and chmod below act on whatever it points to.
# shellcheck source=SCRIPTDIR/../../../usr/lib/ytcast/common.sh
source /usr/lib/ytcast/common.sh
state_dir() {
    local dir="$1" owner="$2"
    if [ -L "${dir}" ]; then
        bashio::exit.nok "${dir} is a symbolic link; remove it and start the app again"
    fi
    mkdir -p "${dir}"
    chown "${owner}:" "${dir}"  # and its login group
    chmod 0700 "${dir}"
}
state_dir /data/ytcast ytcast
state_dir /data/mpd mpd
state_dir /data/mpd/music mpd
state_dir /data/mpd/playlists mpd
state_dir "${YTCAST_ADAPTER_STATE_DIR}" adapter

# MPD's configuration with the stream encoder for stream_format. No secrets in
# it; root's, readable by MPD.
stream_format="$(ytcast_option stream_format)"
encoder="$(ytcast_stream_encoder "${stream_format}")"
template="$(< /etc/ytcast/mpd.conf.template)"
[ "$(grep -c '@STREAM_ENCODER@' <<<"${template}")" = 1 ] \
    || bashio::exit.nok "/etc/ytcast/mpd.conf.template must name @STREAM_ENCODER@ exactly once"
mkdir -p "$(dirname "${YTCAST_MPD_CONF}")"
printf '%s\n' "${template/@STREAM_ENCODER@/${encoder}}" > "${YTCAST_MPD_CONF}"
chmod 0644 "${YTCAST_MPD_CONF}"
