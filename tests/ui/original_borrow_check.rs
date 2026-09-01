use yaspar_macros::stack_safe;

enum Chain<'a> {
    Nil,
    Cons(u64, &'a Chain<'a>),
}

// `data_in_frame` moves the value built at the call site into the driver's store and hands the
// callee a raw pointer to it, so the borrow checker stops seeing that the callee is holding a
// borrow of a temporary. Returning that borrow used to compile. The copy kept for checking is the
// original, so the error the original earns is reported again.
#[stack_safe(data_in_frame)]
fn grow<'a>(n: usize, c: &'a Chain<'a>) -> &'a Chain<'a> {
    if n == 0 { c } else { grow(n - 1, &Chain::Cons(1, c)) }
}

fn main() {}
