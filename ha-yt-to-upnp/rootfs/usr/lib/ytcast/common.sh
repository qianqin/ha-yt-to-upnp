# shellcheck shell=bash
# Helpers sourced by the s6 run scripts.

readonly YTCAST_MPD_HOST=127.0.0.1
readonly YTCAST_MPD_PORT=6600
# Read directly rather than through bashio::config, which asks the Supervisor
# API: the app needs no API access for its own options.
readonly YTCAST_OPTIONS=/data/options.json
# The adapter's state directory: made by ytcast-init, passed by the adapter's run.
# shellcheck disable=SC2034
readonly YTCAST_ADAPTER_STATE_DIR=/data/adapter
# MPD's configuration as ytcast-init writes it for the stream_format option.
# shellcheck disable=SC2034
readonly YTCAST_MPD_CONF=/run/ytcast/mpd.conf

# MPD's httpd encoder settings and the stream's MIME type for stream_format.
# The stream's URL ends in .<stream_format>; MPD ignores the path.
ytcast_stream_encoder() {
    case "$1" in
        # Level 5 is libFLAC's default: a fraction of a core, ~1-1.5 Mbit/s.
        flac) printf '%s\n    %s' 'encoder     "flac"' 'compression "5"' ;;
        mp3) printf '%s\n    %s' 'encoder     "lame"' 'bitrate     "320"' ;;
        *) bashio::exit.nok "unknown stream format '$1'" ;;
    esac
}
ytcast_stream_mime() {
    case "$1" in
        flac) echo audio/flac ;;
        mp3) echo audio/mpeg ;;
        *) bashio::exit.nok "unknown stream format '$1'" ;;
    esac
}

# MPD's configuration: the template (text) with its one @STREAM_ENCODER@
# replaced by the encoder settings. Called directly, not in $(...), so that
# exit.nok ends the caller.
ytcast_mpd_conf() {
    local template="$1" encoder="$2" placeholder='@STREAM_ENCODER@' rest
    # Occurrences, not lines: two on one line would also be wrong.
    rest="${template//"${placeholder}"/}"
    [ $(( (${#template} - ${#rest}) / ${#placeholder} )) = 1 ] \
        || bashio::exit.nok "/etc/ytcast/mpd.conf.template must name @STREAM_ENCODER@ exactly once"
    # Quoted: with bash 5.2's patsub_replacement an & would be the match.
    printf '%s\n' "${template/"${placeholder}"/"${encoder}"}"
}

# True if the option is set (not missing, null or empty).
ytcast_has_option() {
    jq -e --arg k "$1" '.[$k] != null and .[$k] != ""' "${YTCAST_OPTIONS}" >/dev/null
}

# The value of a required option; fails if it is not set.
ytcast_option() {
    if ! ytcast_has_option "$1"; then
        bashio::log.error "option '$1' is not set"
        return 1
    fi
    jq -r --arg k "$1" '.[$k]' "${YTCAST_OPTIONS}"
}

# MPD starts in parallel; a client that starts first would exit and be
# restarted by s6 in a loop, so wait for its control port instead.
ytcast_wait_for_mpd() {
    local tries=60
    until (exec 3<>"/dev/tcp/${YTCAST_MPD_HOST}/${YTCAST_MPD_PORT}") 2>/dev/null; do
        tries=$((tries - 1))
        if [ "${tries}" -le 0 ]; then
            bashio::log.error "MPD does not answer on ${YTCAST_MPD_HOST}:${YTCAST_MPD_PORT}"
            return 1
        fi
        sleep 0.5
    done
}

# The app's log level (debug|info|warning|error) as the given component reads it.
ytcast_log_level() {
    local level
    level="$(ytcast_option log_level)" || return 1
    case "$1:${level}" in
        rust:debug) echo "info,ytcast_lite=debug" ;;  # dependencies stay at info
        rust:warning) echo "warn" ;;
        rust:*) echo "${level}" ;;
        python:*) echo "${level^^}" ;;
        *) bashio::exit.nok "unknown log level target '$1'" ;;
    esac
}

# The LAN IPv4 address of the host: the source address of the default route.
# connect() on a UDP socket only picks the route, it sends no packet.
ytcast_lan_ipv4() {
    python3 -I -c 'import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.connect(("192.0.2.1", 9))
print(s.getsockname()[0])'
}

# Logs why a service ended; s6 restarts it afterwards. The pause keeps a
# service that fails right away from flooding the log.
ytcast_finish() {
    local name="$1" code="$2" signal="$3"
    if [ "${code}" -eq 256 ]; then
        # Stopped by s6 (shutdown or restart of the app).
        [ "${signal}" -eq 15 ] || bashio::log.warning "${name} was killed by signal ${signal}"
        return 0
    fi
    if [ "${code}" -ne 0 ]; then
        bashio::log.warning "${name} exited with code ${code}; restarting in 3 s"
        sleep 3
    else
        bashio::log.warning "${name} exited (code 0); restarting"
    fi
}
