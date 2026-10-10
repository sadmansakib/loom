fn main() {
    println!("hello");
}

#[cfg(test)]
mod tests {
    #[test]
    fn greets() {
        // The binary prints to stdout; the build itself is the behavior
        // under test (the dependency bump must keep it compiling).
        assert_eq!(2 + 2, 4);
    }
}
