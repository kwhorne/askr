//! M0 spike demo: boot PHP in-process and run a snippet.
//!
//!   cargo run -p askr-php --example hello

fn main() {
    let mut php = askr_php::Interpreter::new().expect("failed to init embedded PHP");

    println!("embedded PHP version: {}", php.php_version());

    let script = r#"
        $data = ['stack' => 'TALL', 'server' => 'askr', 'n' => array_sum(range(1, 10))];
        echo "hello from PHP " . PHP_VERSION . "\n";
        echo json_encode($data, JSON_PRETTY_PRINT) . "\n";
    "#;

    let result = php.eval(script).expect("eval");
    print!("{}", result.output);
    println!("[ok={} status={}]", result.ok(), result.status);
}
