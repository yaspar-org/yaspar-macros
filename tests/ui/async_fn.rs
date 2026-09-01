use yaspar_macros::stack_safe;

#[stack_safe]
async fn f(n: u64) -> u64 {
    if n == 0 { 0 } else { f(n - 1).await }
}

fn main() {}
