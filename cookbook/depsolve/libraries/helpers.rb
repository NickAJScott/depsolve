# frozen_string_literal: true

require 'digest'

module Depsolve
  # Locates the server installation and its depsolver worker.
  module Helpers
    STAGING_DIR = '/opt/depsolve'
    INSTALL_ROOTS = %w[/opt/opscode /opt/cinc-project].freeze
    PATCH_MARKER = 'libdepsolve_rust'

    def depsolve_install_root
      root = node['depsolve']['install_root'] ||
             INSTALL_ROOTS.find { |dir| ::File.directory?("#{dir}/embedded/service/opscode-erchef") }
      raise 'depsolve: no Chef Infra Server or CINC Server installation found; set node["depsolve"]["install_root"]' unless root

      root
    end

    # The depselector_rb directory of the newest chef_objects release on disk.
    def depsolve_worker_dir
      pattern = "#{depsolve_install_root}/embedded/service/opscode-erchef/lib/chef_objects-*/priv/depselector_rb"
      newest = ::Dir.glob(pattern).max_by { |path| Gem::Version.new(path[/chef_objects-(\d+(?:\.\d+)*)/, 1] || '0') }
      raise "depsolve: no depsolver worker directory matches #{pattern}" unless newest

      newest
    end

    def depsolve_worker
      "#{depsolve_worker_dir}/depselector.rb"
    end

    def depsolve_stock_backup
      "#{depsolve_worker}.stock"
    end

    def depsolve_patched?
      ::File.exist?(depsolve_worker) && ::File.read(depsolve_worker).include?(PATCH_MARKER)
    end

    def depsolve_restart_command
      node['depsolve']['restart_command'] || "#{depsolve_install_root}/bin/chef-server-ctl restart opscode-erchef"
    end

    def depsolve_wait_command
      "#{depsolve_install_root}/embedded/bin/curl -sS -o /dev/null http://127.0.0.1:#{node['depsolve']['erchef_port']}/_status"
    end
  end
end

Chef::DSL::Recipe.include(Depsolve::Helpers)
Chef::Resource.include(Depsolve::Helpers)
