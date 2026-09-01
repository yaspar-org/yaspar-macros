use yaspar_macros::stack_safe;

struct Parity;

trait Parities {
    fn even(&self, n: u64) -> bool;
    fn odd(&self, n: u64) -> bool;
}

// Two members of the trait impl recursing through each other, which needs the same thing a single
// recursive member needs: somewhere beside them to put the body.
#[stack_safe]
impl Parities for Parity {
    fn even(&self, n: u64) -> bool {
        if n == 0 { true } else { self.odd(n - 1) }
    }

    fn odd(&self, n: u64) -> bool {
        if n == 0 { false } else { self.even(n - 1) }
    }
}

fn main() {}
