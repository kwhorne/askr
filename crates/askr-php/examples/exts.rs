fn main() {
    let mut p = askr_php::Interpreter::new().unwrap();
    println!(
        "{}",
        p.eval("echo implode(\",\", get_loaded_extensions());")
            .unwrap()
            .output
    );
}
