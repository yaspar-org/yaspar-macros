use yaspar_macros::stack_safe;

#[stack_safe]
fn depth<T: Copy>(n: u64, t: T) -> u64 {
    fn step(n: u64) -> u64 {
        if n == 0 { 0 } else { depth(n - 1, 0u8) }
    }
    if n == 0 { 0 } else { 1 + step(n - 1) }
}

fn main() {}
