use yaspar_macros::stack_safe;

#[stack_safe]
fn f(n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        println!("{}", f(n - 1));
        0
    }
}

fn main() {}
