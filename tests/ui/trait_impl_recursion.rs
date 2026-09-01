use yaspar_macros::stack_safe;

struct Tree {
    v: u64,
    kids: Vec<Tree>,
}

trait Bump {
    fn bump(&self, t: &mut Tree) -> u64;
}

// A rewritten member needs a plain associated function beside it to carry the body, and a trait
// impl may hold nothing but the trait's own members.
#[stack_safe(use_nonlinear_mut)]
impl Bump for Tree {
    fn bump(&self, t: &mut Tree) -> u64 {
        t.v += 1;
        let mut n = t.v;
        for i in 0..t.kids.len() {
            n += self.bump(&mut t.kids[i]);
        }
        n
    }
}

fn main() {}
