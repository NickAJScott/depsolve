#!/bin/sh
# End-to-end test of the cookbook against a real CINC Server in Docker.
#
#   1. builds the engine (build.sh) and the server image, starts the server if needed
#   2. converges recipe[depsolve] with the server's own embedded client
#   3. runs the server's depsolver API tests (oc-chef-pedant) against the new worker
#   4. converges recipe[depsolve::remove] and reruns the tests against the stock worker
#
# With the depsolve worker, three pedant examples fail because they assert the stock
# solver's error wording word for word (see README, "Differences"). The test checks
# that those three are the only failures and that each differs only in the message.
# The first server start takes around ten minutes under amd64 emulation.
set -eu
cd "$(dirname "$0")/.."

container=depsolve-cinc-server
image=depsolve-cinc-server
output=$(mktemp)

./build.sh

if ! docker inspect "$container" >/dev/null 2>&1; then
  docker build -t "$image" test/cinc-server
  docker run -d --name "$container" --platform linux/amd64 --privileged --hostname cinc.test "$image" >/dev/null
fi
docker start "$container" >/dev/null
echo "waiting for the server (docker logs -f $container shows progress)..."
until docker logs "$container" 2>&1 | grep -q "cinc-server ready"; do
  if docker logs "$container" 2>&1 | grep -q "cinc-server reconfigure failed"; then
    echo "server configuration failed; see docker logs $container" >&2
    exit 1
  fi
  sleep 10
done

docker exec "$container" rm -rf /tmp/depsolve-test
docker exec "$container" mkdir -p /tmp/depsolve-test/cookbooks
docker cp cookbook/depsolve "$container:/tmp/depsolve-test/cookbooks/depsolve" >/dev/null
# erchef ignores the graceful stop signal under amd64 emulation, so the rig forces the restart.
docker exec -i "$container" sh -c 'cat > /tmp/depsolve-test/attributes.json' <<'JSON'
{"depsolve": {"restart_command": "/opt/cinc-project/embedded/bin/sv -w 15 force-restart /opt/cinc-project/service/opscode-erchef || true"}}
JSON

converge() {
  docker exec "$container" /opt/cinc-project/embedded/bin/cinc-client --local-mode --chef-license accept \
    --config-option cookbook_path=/tmp/depsolve-test/cookbooks --config-option file_cache_path=/tmp/depsolve-test/cache \
    --json-attributes /tmp/depsolve-test/attributes.json --override-runlist "$1" --log_level warn
}

# Runs the server's depsolver API tests and prints their output, colour codes removed.
pedant() {
  docker exec "$container" cinc-server-ctl test --focus depsolver 2>&1 | sed 's/\x1b\[[0-9;]*m//g' || true
}

echo "== converge recipe[depsolve]"
converge 'recipe[depsolve]'
echo "== converge again (must change nothing)"
second=$(converge 'recipe[depsolve]')
echo "$second"
echo "$second" | grep -q "0/[0-9]* resources updated" || { echo "second converge was not idempotent" >&2; exit 1; }

echo "== pedant depsolver tests, depsolve worker"
# Only the three wording examples may fail, and only on the message (see wording_failures.rb).
pedant > "$output"
ruby_in_container=/opt/cinc-project/embedded/bin/ruby
docker cp test/wording_failures.rb "$container:/tmp/depsolve-test/wording_failures.rb" >/dev/null
docker cp "$output" "$container:/tmp/depsolve-test/pedant.log" >/dev/null
docker exec "$container" "$ruby_in_container" /tmp/depsolve-test/wording_failures.rb /tmp/depsolve-test/pedant.log || {
  echo "full test output: $output" >&2
  exit 1
}

echo "== converge recipe[depsolve::remove]"
converge 'recipe[depsolve::remove]'

echo "== pedant depsolver tests, stock worker"
pedant > "$output"
summary=$(grep -E '^[0-9]+ examples, [0-9]+ failures' "$output" | tail -1)
echo "${summary:-no summary} (expected 0 failures)"
case "$summary" in
  *", 0 failures"*) ;;
  *) echo "full test output: $output" >&2; exit 1 ;;
esac

rm -f "$output"
echo "PASS"
