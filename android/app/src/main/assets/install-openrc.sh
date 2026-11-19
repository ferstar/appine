# Runs as root before the first Alpine boot, while the login bundles are still staged.
set -eu
apk_offline() {
    apk --no-network --repositories-file /dev/null "$@"
}

# Use a temporary world entry for the local files so future upgrades can resolve the
# dependencies by name. A retry can resume after either package transaction.
if ! apk info -e openrc >/dev/null; then
    apk_offline add --virtual .appine-openrc /var/cache/appine/openrc/*.apk \
        /var/cache/appine/bubblewrap/libcap2-*.apk
fi
apk_offline add openrc
if apk info -e .appine-openrc >/dev/null; then
    apk_offline del .appine-openrc
fi
