# Installs the depsolve worker in place of the server's stock depsolver worker.
#
# Order matters: the engine and worker are staged and self-tested with the server's
# own Ruby before anything the server uses is touched. If the self-test fails, the
# run stops with the stock worker still in place.

staging = Depsolve::Helpers::STAGING_DIR
library = "#{staging}/libdepsolve_rust.so"
staged_worker = "#{staging}/depselector.rb"
self_test_stamp = "#{staging}/.self-test-passed"
worker = depsolve_worker
worker_dir = depsolve_worker_dir
root = depsolve_install_root

unless depsolve_patched?
  checksum = Digest::SHA256.file(worker).hexdigest
  unless node['depsolve']['known_stock_checksums'].include?(checksum) || node['depsolve']['allow_unknown_stock_worker']
    raise "depsolve: #{worker} (sha256 #{checksum}) is not a stock worker this cookbook has been tested against. " \
          'Check that your server version passes the test rig, then add the checksum to ' \
          'node["depsolve"]["known_stock_checksums"] or set node["depsolve"]["allow_unknown_stock_worker"].'
  end
end

directory staging do
  owner 'root'
  group 'root'
  mode '0755'
end

if node['depsolve']['library_url']
  raise 'depsolve: node["depsolve"]["library_checksum"] is required with library_url' unless node['depsolve']['library_checksum']

  remote_file library do
    source node['depsolve']['library_url']
    checksum node['depsolve']['library_checksum']
    owner 'root'
    group 'root'
    mode '0755'
    notifies :run, 'execute[restart erchef for depsolve]', :delayed
  end
else
  cookbook_file library do
    source 'libdepsolve_rust.so'
    owner 'root'
    group 'root'
    mode '0755'
    notifies :run, 'execute[restart erchef for depsolve]', :delayed
  end
end

cookbook_file staged_worker do
  source 'depselector.rb'
  owner 'root'
  group 'root'
  mode '0644'
end

# Identifies the staged engine and worker, so the self-test reruns when either changes.
staged_fingerprint = lambda do
  [library, staged_worker].map { |path| Digest::SHA256.file(path).hexdigest }.join("\n")
end

ruby_block 'depsolve self-test' do
  block do
    test = Mixlib::ShellOut.new(
      "#{root}/embedded/bin/ruby #{staged_worker} --self-test",
      environment: { 'DEPSOLVE_LIB' => library, 'DEPSOLVE_BUNDLE_DIR' => worker_dir },
      timeout: 120
    )
    test.run_command
    test.error!
    ::File.write(self_test_stamp, staged_fingerprint.call)
  end
  not_if { ::File.exist?(self_test_stamp) && ::File.read(self_test_stamp) == staged_fingerprint.call }
end

# Keep the stock worker beside the replacement so depsolve::remove can restore it.
execute 'back up stock depsolver worker' do
  command "cp -p #{worker} #{depsolve_stock_backup}"
  not_if { depsolve_patched? }
end

cookbook_file worker do
  source 'depselector.rb'
  owner 'root'
  group 'root'
  mode '0644'
  notifies :run, 'execute[restart erchef for depsolve]', :delayed
end

execute 'restart erchef for depsolve' do
  command depsolve_restart_command
  action :nothing
  notifies :run, 'execute[wait for erchef]', :immediately
end

execute 'wait for erchef' do
  command depsolve_wait_command
  retries 90
  retry_delay 2
  action :nothing
end
