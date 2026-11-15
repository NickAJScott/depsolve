# Ruby binding

[solver_bridge.rb](solver_bridge.rb) calls the Rust solver through its C ABI (`depsolve_solve` / `depsolve_free`) using the stdlib `fiddle` module. The problem and outcome formats are described in the [top-level README](../README.md#problem-format).

## Build the library

```bash
cargo build --release --manifest-path rust/Cargo.toml
```

Set `DEPSOLVE_LIB` to use a library from another location.

## Run the example

```bash
ruby ruby/solver_bridge.rb
```

## Use it

```ruby
require_relative 'ruby/solver_bridge'

outcome = Depsolve::RustBridge.solve(
  'cookbooks' => { 'app' => { '1.1.0' => { 'lib' => '~> 1.0' } }, 'lib' => { '1.5.0' => {} } },
  'run_list' => { 'app' => '= 1.1.0' }
)
outcome['status']   # 'ok', 'unsolvable' or 'error'
```

`solve` raises `Depsolve::RustBridge::LibraryNotFound` if the library has not been built. Solver failures are returned in the outcome, never swallowed.
