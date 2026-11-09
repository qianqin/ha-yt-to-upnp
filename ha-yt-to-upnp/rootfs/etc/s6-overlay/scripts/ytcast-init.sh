#!/command/with-contenv bashio
# shellcheck shell=bash
# Persistent state under /data, private to the user of each service.
set -e

mkdir -p /data/ytcast /data/mpd/music /data/mpd/playlists
chown -R ytcast:ytcast /data/ytcast
chown -R mpd: /data/mpd
chmod 0700 /data/ytcast /data/mpd
