use yaspar_macros::stack_safe;

#[stack_safe]
struct NotAFunction(u64);

fn main() {}
