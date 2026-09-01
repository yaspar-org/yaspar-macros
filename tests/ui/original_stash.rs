use yaspar_macros::stack_safe;

enum Chain<'a> {
    Nil,
    Cons(u64, &'a Chain<'a>),
}

// The callee stashes a borrow of the value lent to it, which outlives the call that lent it. The
// driver's store keeps that value only until the frame that built it is popped, so this is the
// other half of the invariant `data_in_frame` would otherwise leave to the caller.
#[stack_safe(data_in_frame)]
fn stash<'a>(n: usize, c: &'a Chain<'a>, out: &mut Vec<&'a Chain<'a>>) -> usize {
    if n == 0 {
        out.len()
    } else {
        out.push(c);
        stash(n - 1, &Chain::Cons(1, c), out)
    }
}

fn main() {}
