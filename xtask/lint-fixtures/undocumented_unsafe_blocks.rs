unsafe fn callee() {}

fn fixture() {
    unsafe { callee() }
}
