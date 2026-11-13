fn main() {
    vera_build::Build::new("policy.vera")
        .module_name("verified")
        .compile()
        .expect("VERA core must verify before this application can build");
}
