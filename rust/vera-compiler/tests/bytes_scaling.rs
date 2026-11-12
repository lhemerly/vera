use vera_compiler::{
    parse_source,
    semantic::analyze,
    verifier::{verify_all, VerifierConfig},
};
#[test]
fn ordinary_packet_roundtrip_has_no_capacity_case_explosion() {
    for capacity in [4, 8, 16, 32] {
        for record in [false, true] {
            let (definition, ty) = if record {
                (
                    format!("record Packet {{kind:u8;content:bytes<{capacity}>;}}"),
                    "Packet".to_string(),
                )
            } else {
                (String::new(), format!("bytes<{capacity}>"))
            };
            let source=format!("{definition} function roundtrip(x:{ty})->{ty} ensure exact:eq(result,x);body {{yield decode<{ty}>(encode<{ty}>(x));}}");
            let p = analyze(parse_source(&source).unwrap()).unwrap();
            let now = std::time::Instant::now();
            let results = verify_all(
                &p,
                &VerifierConfig {
                    timeout_ms: Some(10_000),
                    ..Default::default()
                },
            );
            eprintln!(
                "capacity {capacity}, record {record}: {:?}: {}",
                now.elapsed(),
                results[0].summary()
            );
            assert!(results[0].ok(), "{}", results[0].summary());
        }
    }
}
