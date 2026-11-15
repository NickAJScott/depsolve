#!/bin/sh
# Starts the runit supervisor, configures the server on first boot, then stays up.
# The container keeps running if configuration fails, so it can be inspected and
# `cinc-server-ctl reconfigure` rerun by hand.

# Services stop slowly under amd64 emulation; give `sv restart` longer than its 7s default.
export SVWAIT=180
# reconfigure installs a log-rotation job here; the base image has no cron directories.
mkdir -p /etc/cron.hourly

/opt/cinc-project/embedded/bin/runsvdir-start &

if [ ! -f /var/opt/cinc-project/.depsolve-configured ]; then
  if cinc-server-ctl reconfigure --chef-license=accept; then
    touch /var/opt/cinc-project/.depsolve-configured
    echo "cinc-server ready"
  else
    echo "cinc-server reconfigure failed"
  fi
else
  cinc-server-ctl start
  echo "cinc-server ready"
fi

exec tail -f /dev/null
