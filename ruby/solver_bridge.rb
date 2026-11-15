# frozen_string_literal: true

require 'json'
require 'fiddle'

module Depsolve
  # Calls the Rust solver (rust/src/lib.rs) through its C ABI.
  class RustBridge
    class LibraryNotFound < StandardError; end

    LIBRARY_NAMES = %w[libdepsolve_rust.dylib libdepsolve_rust.so].freeze

    def self.library_path
      @library_path ||= begin
        candidates = [ENV['DEPSOLVE_LIB']].compact
        %w[release debug].each do |profile|
          LIBRARY_NAMES.each do |name|
            candidates << File.expand_path("../rust/target/#{profile}/#{name}", __dir__)
          end
        end
        candidates.find { |path| File.exist?(path) } ||
          raise(LibraryNotFound, 'Rust solver library not found; build it with: ' \
                                 'cargo build --release --manifest-path rust/Cargo.toml')
      end
    end

    def self.handle
      @handle ||= Fiddle::Handle.new(library_path)
    end

    # problem: { 'cookbooks' => { name => { version => { dep => constraint } } },
    #            'run_list' => { name => constraint },
    #            'environment' => { name => constraint },    # optional
    #            'timeout_ms' => Integer }                   # optional
    #
    # Returns a hash whose 'status' is 'ok', 'unsolvable', 'invalid_run_list',
    # 'timeout' or 'error'; see the top-level README for the fields of each.
    def self.solve(problem)
      solve_function = Fiddle::Function.new(handle['depsolve_solve'], [Fiddle::TYPE_VOIDP], Fiddle::TYPE_VOIDP)
      free_function = Fiddle::Function.new(handle['depsolve_free'], [Fiddle::TYPE_VOIDP], Fiddle::TYPE_VOID)

      result_ptr = solve_function.call(JSON.generate(problem))
      begin
        JSON.parse(result_ptr.to_s.force_encoding(Encoding::UTF_8))
      ensure
        free_function.call(result_ptr)
      end
    end
  end
end

if __FILE__ == $PROGRAM_NAME
  problem = {
    'cookbooks' => {
      'app' => { '1.0.0' => {}, '1.1.0' => { 'lib' => '~> 1.0' } },
      'lib' => { '1.0.0' => {}, '1.5.0' => {}, '2.0.0' => {} }
    },
    'run_list' => { 'app' => '= 1.1.0' }
  }

  puts JSON.pretty_generate(Depsolve::RustBridge.solve(problem))
end
