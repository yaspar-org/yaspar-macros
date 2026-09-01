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
| `Step`, `In`          | the protocol between a rewritten body and its driver                                                           |
| `drive`               | the loop that keeps the recursion in a `Vec` instead of on the native stack                                    |
| `Pin`                 | the store for values a call site lends its callee, under `#[stack_safe(data_in_frame)]`                        |
| `Try`, `FromResidual` | a stand-in for the unstable traits of the same names, so that `?` works on a `Result` and on an `Option` alike |

A rewritten body imports all of them at its top, under `__ss` names, so an expansion reads the same as it did when they
were emitted into it.

See the [`yaspar-macros` README](../README.md) for what the transformation does, what it preserves, and what it rejects.
