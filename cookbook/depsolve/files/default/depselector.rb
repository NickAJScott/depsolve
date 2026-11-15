# frozen_string_literal: true

# Drop-in replacement for erchef's priv/depselector_rb/depselector.rb.
#
# erchef keeps a pool of these workers and talks to each over an Erlang port. This
# version speaks the same protocol and returns the same reply shapes, but solves with
# the depsolve Rust engine (PubGrub) instead of dep_selector/Gecode. Nothing on the
# Erlang side changes. Installed and removed by the depsolve cookbook.
#
#   ruby depselector.rb --self-test   load the engine, solve one problem, exit 0 or 1
#
# Environment:
#   DEPSOLVE_LIB         path of the engine library (default /opt/depsolve/libdepsolve_rust.so)
#   DEPSOLVE_BUNDLE_DIR  directory holding the server's depselector Gemfile (default:
#                        the directory this file is in)

# The server's Gemfile must be in the cwd for bundler/setup, as in the original.
Dir.chdir(ENV.fetch('DEPSOLVE_BUNDLE_DIR') { File.dirname(File.realpath(__FILE__)) })

require 'rubygems'
require 'bundler/setup'
require 'erlectricity'
require 'fiddle'
require 'json'

LIBRARY = Fiddle::Handle.new(ENV.fetch('DEPSOLVE_LIB', '/opt/depsolve/libdepsolve_rust.so'))
SOLVE = Fiddle::Function.new(LIBRARY['depsolve_solve'], [Fiddle::TYPE_VOIDP], Fiddle::TYPE_VOIDP)
FREE = Fiddle::Function.new(LIBRARY['depsolve_free'], [Fiddle::TYPE_VOIDP], Fiddle::TYPE_VOID)

OPERATORS = { gt: '>', gte: '>=', lt: '<', lte: '<=', eq: '=', pes: '~>' }.freeze

# erchef sends constraints as {name, version, operator}, {name, version} or a bare name.
def constraint_string(operator, version)
  return '' unless version

  operator = case operator
             when Symbol then OPERATORS.fetch(operator, operator.to_s)
             when nil then '='
             else operator.to_s
             end
  "#{operator} #{version}"
end

def monotonic_ms
  Process.clock_gettime(Process::CLOCK_MONOTONIC, :millisecond)
end

def build_problem(data)
  cookbooks = data[:all_versions].to_h do |name, versions|
    [name, versions.to_h do |version, dependencies|
      [version, dependencies.to_h { |dependency, dep_version, op| [dependency, constraint_string(op, dep_version)] }]
    end]
  end
  environment = data[:environment_constraints].to_h do |name, version, op|
    [name, constraint_string(op, version)]
  end
  # Run-list order is kept: it decides which item is blamed when there is no solution.
  run_list = data[:run_list].map do |item|
    name, version, op = item
    [name, constraint_string(op, version)]
  end

  { 'cookbooks' => cookbooks, 'run_list' => run_list, 'environment' => environment }
end

# The API reports each most-constrained cookbook as "name = version -> [(dep op version), ...]",
# using the first version erchef listed for it.
def describe_most_constrained(name, data)
  _, versions = data[:all_versions].find { |cookbook, _| cookbook == name }
  return name if versions.nil? || versions.empty?

  version, dependencies = versions.first
  described = dependencies.map do |dependency, dep_version, op|
    wanted = constraint_string(op, dep_version)
    "(#{dependency} #{wanted.empty? ? '>= 0.0.0' : wanted})"
  end
  "#{name} = #{version} -> [#{described.join(', ')}]"
end

def solve(data)
  received = monotonic_ms
  problem = build_problem(data)
  # erchef's timeout covers the whole request, so take off what building the problem cost.
  problem['timeout_ms'] = [data[:timeout_ms] - (monotonic_ms - received), 0].max if data[:timeout_ms]

  pointer = SOLVE.call(JSON.generate(problem))
  outcome = begin
    JSON.parse(pointer.to_s.force_encoding(Encoding::UTF_8))
  ensure
    FREE.call(pointer)
  end

  case outcome['status']
  when 'ok'
    packages = Erl::List.new
    outcome['solution'].each { |name, version| packages << [name, version.split('.').map(&:to_i)] }
    [:ok, packages]
  when 'unsolvable'
    most_constrained = outcome['most_constrained_cookbooks'].map { |name| describe_most_constrained(name, data) }
    detail = Erl::List.new([[:message, outcome['explanation']],
                            [:unsatisfiable_run_list_item, outcome['unsatisfiable_run_list_item']],
                            [:non_existent_cookbooks, Erl::List.new(outcome['non_existent_cookbooks'])],
                            [:most_constrained_cookbooks, Erl::List.new(most_constrained)]])
    [:error, :no_solution, detail]
  when 'invalid_run_list'
    detail = Erl::List.new([[:non_existent_cookbooks, Erl::List.new(outcome['non_existent_cookbooks'])],
                            [:constraints_not_met, Erl::List.new(outcome['no_versions'])]])
    [:error, :invalid_constraints, detail]
  when 'timeout'
    [:error, :resolution_timeout]
  else
    [:error, :exception, outcome['message'].to_s, Erl::List.new]
  end
end

# Solves a few fixed requests and checks the replies erchef would receive.
def self_test
  universe = [['app', [['1.0.0', [['lib', '1.0.0', :gte]]]]],
              ['lib', [['1.2.0', []], ['0.9.0', []]]],
              ['needy', [['2.0.0', [['lib', '5.0.0', :gt]]]]]]
  request = lambda do |run_list, timeout_ms|
    solve(all_versions: universe, environment_constraints: [], run_list: run_list, timeout_ms: timeout_ms)
  end
  failures = []

  status, packages = request.call(['app'], 5000)
  failures << "solve: #{[status, packages].inspect}" unless status == :ok && packages.sort == [['app', [1, 0, 0]], ['lib', [1, 2, 0]]]

  reply = request.call(['app'], 0)
  failures << "timeout: #{reply.inspect}" unless reply == [:error, :resolution_timeout]

  status, kind, detail = request.call(%w[app needy], 5000)
  detail = detail.to_h if detail
  unless status == :error && kind == :no_solution &&
         detail[:unsatisfiable_run_list_item] == '(needy >= 0.0.0)' &&
         detail[:most_constrained_cookbooks] == ['lib = 1.2.0 -> []'] &&
         detail[:non_existent_cookbooks] == []
    failures << "no solution: #{[status, kind, detail].inspect}"
  end

  status, kind, detail = request.call([['app', '9.9.9']], 5000)
  detail = detail.to_h if detail
  unless status == :error && kind == :invalid_constraints && detail[:constraints_not_met] == ['(app = 9.9.9)']
    failures << "invalid run list: #{[status, kind, detail].inspect}"
  end

  abort "depsolve self-test failed:\n#{failures.join("\n")}" unless failures.empty?
  puts 'depsolve self-test passed'
end

if ARGV.first == '--self-test'
  self_test
  exit 0
end

receive do |m|
  m.when([:get_pid]) do
    m.send!(Process.pid)
    m.receive_loop
  end

  m.when([:solve, Erl.hash]) do |data|
    answer = begin
      solve(data)
    rescue StandardError => e
      [:error, :exception, e.message, Erl::List.new(e.backtrace)]
    end
    m.send!(answer)
    m.receive_loop
  end
end
