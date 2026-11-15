# frozen_string_literal: true

# Checks the output of the server's depsolver API tests (oc-chef-pedant) run against
# the depsolve worker. The only failures allowed are the three examples that assert
# the stock solver's "no solution" message word for word, and each of them must fail
# on the message alone: every other field of the response has to match the spec.
#
#   ruby wording_failures.rb PEDANT_OUTPUT_FILE

EXPECTED = [
  "(one cookbook) returns 412 when there is a dep that doesn't exist",
  "(two cookbooks) returns 412 when there is a dep that doesn't have new enough version",
  '(two cookbooks) returns 412 when there is an impossible dependency'
].freeze

MESSAGE = /"message"=>\s*"(?:[^"\\]|\\.)*",?/

def without_message(text)
  text.gsub(MESSAGE, '').gsub(/\s+/, '')
end

output = File.read(ARGV.fetch(0)).gsub(/\e\[[0-9;]*m/, '')
problems = []

# The run prints two summaries: one for its set-up pass, then the depsolver examples.
summary = output.scan(/^\d+ examples, \d+ failures/).last
problems << 'no rspec summary line; the tests did not run to completion' unless summary

failures = output[/^Failures:\n(.*?)^Finished in/m, 1].to_s.split(/^  \d+\) /).drop(1)
names = failures.map { |block| block.lines.first.strip }

EXPECTED.each do |wanted|
  problems << "expected to fail on wording, but did not fail: #{wanted}" unless names.any? { |name| name.end_with?(wanted) }
end

failures.each do |block|
  name = block.lines.first.strip
  unless EXPECTED.any? { |wanted| name.end_with?(wanted) }
    problems << "unexpected failure: #{name}"
    next
  end

  actual = block[/Expected a full match of the result(.*?)^\s*to the spec/m, 1]
  spec = block[/^\s*to the spec(.*?)^\s*to succeed/m, 1]
  if actual.nil? || spec.nil?
    problems << "failed for a reason other than the response body: #{name}"
  elsif !actual.match?(MESSAGE) || !spec.match?(MESSAGE)
    problems << "no message field to compare: #{name}"
  elsif without_message(actual) != without_message(spec)
    problems << "differs from the spec in more than the message: #{name}"
  elsif actual.gsub(/\s+/, '') == spec.gsub(/\s+/, '')
    problems << "failed although the response matches the spec: #{name}"
  end
end

if problems.empty?
  puts "#{summary}: the #{EXPECTED.size} expected examples, each differing from the spec only in the message"
else
  puts summary if summary
  problems.each { |problem| puts "  #{problem}" }
  exit 1
end
