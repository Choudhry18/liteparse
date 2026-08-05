fn main() {
    for path in std::env::args().skip(1) {
        let bytes = std::fs::read(&path).unwrap();
        match liteparse_docx::docx::parse(&bytes) {
            Ok(_) => println!("OK   {path}"),
            Err(e) => println!("FAIL {path}: {e}"),
        }
    }
}
