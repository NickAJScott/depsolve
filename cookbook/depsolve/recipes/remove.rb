# Restores the server's stock depsolver worker and removes depsolve.

worker = depsolve_worker
stock = depsolve_stock_backup

if depsolve_patched? && !::File.exist?(stock)
  raise "depsolve: #{worker} is the depsolve worker but #{stock} is missing. " \
        'Reinstall the server package to get the stock worker back.'
end

execute 'restore stock depsolver worker' do
  command "cp -p #{stock} #{worker}"
  only_if { depsolve_patched? }
  notifies :run, 'execute[restart erchef for depsolve]', :immediately
end

file stock do
  action :delete
end

directory Depsolve::Helpers::STAGING_DIR do
  recursive true
  action :delete
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
