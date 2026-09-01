use yaspar_macros::stack_safe;

#[stack_safe]
mod m {
    pub fn up(n: u64) -> impl Iterator<Item = u64> {
        if n == 0 { 0..1 } else { let k = down(n - 1); 0..k }
    }

    pub fn down(n: u64) -> u64 {
        if n == 0 { 1 } else { up(n - 1).count() as u64 }
    }
}

fn main() {}
