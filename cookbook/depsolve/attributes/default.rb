# Where the server is installed. Detected when nil: /opt/opscode (Chef Infra Server)
# or /opt/cinc-project (CINC Server).
default['depsolve']['install_root'] = nil

# Where to get libdepsolve_rust.so. With no URL, the cookbook ships its own copy from
# files/default/ (put there by build.sh). With a URL, a SHA-256 checksum is required.
default['depsolve']['library_url'] = nil
default['depsolve']['library_checksum'] = nil

# SHA-256 of every stock depselector.rb this worker has been tested as a replacement
# for. The recipe refuses to replace a worker it does not recognise, because a different
# stock worker may mean erchef speaks a different protocol to it.
default['depsolve']['known_stock_checksums'] = [
  'd66f708518d3eb9920b025d223342035027ff035eab9c87f43e682107b7196a5', # CINC Server 15.10.127
]
# Set to true to replace an unrecognised stock worker anyway. The self-test still runs.
default['depsolve']['allow_unknown_stock_worker'] = false

# erchef has to restart to pick up a different worker. Defaults to
# "<install_root>/bin/chef-server-ctl restart opscode-erchef".
default['depsolve']['restart_command'] = nil
# erchef's HTTP port, polled after the restart until it answers.
default['depsolve']['erchef_port'] = 8000
