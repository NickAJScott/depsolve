# frozen_string_literal: true

# Runs CINC Server's own dependency solver on depsolve problems.
#
# Reads one JSON problem per line on stdin (the format in the top-level README) and
# writes one JSON outcome per line on stdout, each with a "seconds" field.
#
# The graph is built the same way erchef's depselector.rb builds it (same gem bundle,
# same Gecode build, environment constraints applied as a version filter); only the
# Erlang port protocol is replaced by JSON.

DEPSELECTOR_DIR = Dir.glob('/opt/cinc-project/embedded/service/opscode-erchef/lib/chef_objects-*/priv/depselector_rb').first
abort 'depselector_rb not found in the CINC Server package' unless DEPSELECTOR_DIR

# The server's Gemfile must be in the cwd for bundler/setup, as in depselector.rb.
Dir.chdir(DEPSELECTOR_DIR)

require 'rubygems'
require 'bundler/setup'
require 'dep_selector'
require 'json'

# erchef's default depsolver_timeout.
TIMEOUT_MS = Integer(ENV.fetch('CINC_TIMEOUT_MS', '5000'))

def constraint(raw)
  text = raw.to_s.strip
  DepSelector::VersionConstraint.new(text.empty? ? nil : text)
end

def solve(problem)
  graph = DepSelector::DependencyGraph.new
  environment = (problem['environment'] || {}).transform_values { |raw| constraint(raw) }

  all_versions = problem['cookbooks'].map do |name, versions|
    versions.each do |version, dependencies|
      parsed = DepSelector::Version.new(version)
      next if environment[name] && !environment[name].include?(parsed)

      package_version = graph.package(name).add_version(parsed)
      dependencies.each do |dependency, raw|
        package_version.dependencies << DepSelector::Dependency.new(graph.package(dependency), constraint(raw))
      end
    end
    graph.package(name)
  end

  run_list = problem['run_list'].map do |name, raw|
    DepSelector::SolutionConstraint.new(graph.package(name), constraint(raw))
  end

  selector = DepSelector::Selector.new(graph, TIMEOUT_MS / 1000.0)
  solution = selector.find_solution(run_list, all_versions)
  { 'status' => 'ok', 'solution' => solution.transform_values(&:to_s) }
rescue DepSelector::Exceptions::InvalidSolutionConstraints => e
  missing = e.non_existent_packages.map { |item| item.package.name }
  unmet = e.constrained_to_no_versions.map(&:to_s)
  { 'status' => 'unsolvable',
    'explanation' => "invalid run list: non-existent cookbooks #{missing}, constraints not met #{unmet}" }
rescue DepSelector::Exceptions::NoSolutionExists => e
  { 'status' => 'unsolvable', 'explanation' => e.message }
rescue DepSelector::Exceptions::TimeBoundExceeded, DepSelector::Exceptions::TimeBoundExceededNoSolution
  { 'status' => 'timeout', 'message' => "no answer within #{TIMEOUT_MS}ms" }
rescue StandardError => e
  { 'status' => 'error', 'message' => "#{e.class}: #{e.message}" }
end

$stdout.sync = true
$stdin.each_line do |line|
  next if line.strip.empty?

  start = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  outcome = begin
    solve(JSON.parse(line))
  rescue JSON::ParserError => e
    { 'status' => 'error', 'message' => "invalid problem JSON: #{e.message}" }
  end
  outcome['seconds'] = Process.clock_gettime(Process::CLOCK_MONOTONIC) - start
  puts JSON.generate(outcome)
end
