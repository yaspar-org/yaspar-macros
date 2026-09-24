# yaspar-macros-defs

The fixed half of what [`yaspar-macros`](../README.md) expands to.

Nothing here is meant to be written by hand. A proc-macro crate may export nothing but macros, so the definitions that
`#[stack_safe]` expansions refer to cannot live in `yaspar-macros` itself and live here instead. A crate using those
macros
therefore depends on both:

```toml
[dependencies]
yaspar-macros = "0.1"
yaspar-macros-defs = "0.1"
```

An expansion has two halves. One is particular to the function being rewritten: the entry enum has a variant per entry
point and the frame enum a variant per call site, both carrying payloads whose types only that function's body implies.
Those are generated. The other half is the same for every function, so it is written once, here:

| item                  | what it is                                                                                                     |
|-----------------------|----------------------------------------------------------------------------------------------------------------|
| `In`                  | the loop's state: enter an entry point, or resume a frame with the value a callee produced                      |
| `Frames`              | the stack it parks frames on instead of using the native one                                                   |
| `Pin`                 | the store for values a call site lends its callee, under `#[stack_safe(data_in_frame)]`                        |
| `Try`, `FromResidual` | a stand-in for the unstable traits of the same names, so that `?` works on a `Result` and on an `Option` alike |
| `Step`, `drive`       | the same machine as a loop the body is handed to, which is what an expansion used to be; kept as the reference its benchmarks measure against |

A rewritten body imports the ones it turns out to need at its top, under `__ss` names, so an expansion reads the same as
it did when they were emitted into it — a function with no `?` names no `Try`, and one that lends no value names no
`Pin`. The loop itself is written into the rewritten function rather than being called here; see `PERFORMANCE.md` for
what that is worth and why.

See the [`yaspar-macros` README](../README.md) for what the transformation does, what it preserves, and what it rejects.
