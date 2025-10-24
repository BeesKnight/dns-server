use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let workers = num_cpus::get();
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let macro_src = format!(
        "macro_rules! worker_test {{ ($name:ident, $body:block) => {{ #[tokio::test(flavor = \"multi_thread\", worker_threads = {workers})] async fn $name() $body }}; }}\n"
    );
    fs::write(out_dir.join("worker_test_macro.rs"), macro_src).expect("write worker test macro");
}
