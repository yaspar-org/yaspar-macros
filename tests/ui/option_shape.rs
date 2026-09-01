use yaspar_macros::stack_safe;

// The outer attribute never reaches this check, since rustc rejects a key-value invocation of a
// macro attribute itself. A marker on a function inside the scope is read by the scan instead.
#[stack_safe]
fn host(n: u64) -> u64 {
    #[stack_safe = "data_in_frame"]
    fn inner(n: u64) -> u64 {
        if n == 0 { 0 } else { inner(n - 1) }
    }
    inner(n)
}

fn main() {}
