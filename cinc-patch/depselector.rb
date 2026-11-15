# frozen_string_literal: true

# Drop-in replacement for erchef's priv/depselector_rb/depselector.rb.
#
# erchef keeps a pool of these workers and talks to each over an Erlang port. This
# version speaks the same protocol and returns the same reply shapes, but solves with
# the depsolve Rust engine (PubGrub) instead of dep_selector/Gecode. Nothing on the
# Erlang side changes. Install and remove it with depsolve-patch (see README).

# The server's Gemfile must be in the cwd for bundler/setup, as in the original.
Dir.chdir(File.dirname(File.realpath(__FILE__)))

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

def build_problem(data)
  cookbooks = data[:all_versions].to_h do |name, versions|
    [name, versions.to_h do |version, dependencies|
      [version, dependencies.to_h { |dependency, dep_version, op| [dependency, constraint_string(op, dep_version)] }]
    end]
  end
  environment = data[:environment_constraints].to_h do |name, version, op|
    [name, constraint_string(op, version)]
  end

  run_list = {}
  data[:run_list].each do |item|
    name, version, op = item
    wanted = constraint_string(op, version)
    existing = run_list[name]
    # The same cookbook pinned two different ways cannot be satisfied.
    return nil if existing && !existing.empty? && !wanted.empty? && existing != wanted

    run_list[name] = wanted if existing.nil? || existing.empty?
  end

  problem = { 'cookbooks' => cookbooks, 'run_list' => run_list, 'environment' => environment }
  problem['timeout_ms'] = data[:timeout_ms] if data[:timeout_ms]
  problem
end

def no_solution(message, non_existent = [])
  detail = Erl::List.new([[:message, message],
                          [:unsatisfiable_run_list_item, ''],
                          [:non_existent_cookbooks, Erl::List.new(non_existent)],
                          [:most_constrained_cookbooks, Erl::List.new]])
  [:error, :no_solution, detail]
end

def solve(data)
  problem = build_problem(data)
  return no_solution('The run list asks for two different versions of the same cookbook.') unless problem

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
    no_solution(outcome['explanation'], outcome['non_existent_cookbooks'])
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
