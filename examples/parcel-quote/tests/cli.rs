use std::io::Write;
use std::process::{Command, Stdio};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_parcel-quote"))
}

#[test]
fn verified_price_is_the_observable_result() {
    for (weight, express, subtotal, fee) in [
        (1, false, 0, 500),
        (1000, false, 4999, 500),
        (1000, false, 5000, 0),
        (20000, true, 0, 1500),
        (20000, false, u64::MAX, 0),
    ] {
        let output = binary()
            .args([
                "quote",
                &weight.to_string(),
                &express.to_string(),
                &subtotal.to_string(),
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{{\"shipping_cents\":{fee}}}\n")
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn input_domains_and_host_errors_are_visible() {
    for fields in [
        ["0", "false", "0"],
        ["20001", "true", "0"],
        ["-1", "false", "0"],
        ["1000", "yes", "0"],
        ["1", "false", "oops"],
    ] {
        let out = binary().arg("quote").args(fields).output().unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
        assert!(!out.stderr.is_empty());
    }
}

#[test]
fn batch_uses_each_decision_and_rejects_partial_output() {
    for (input, expected) in [
        ("1000,false,4000\n1000,false,5000\n1000,true,5000\n", Some("{\"line\":1,\"shipping_cents\":500}\n{\"line\":2,\"shipping_cents\":0}\n{\"line\":3,\"shipping_cents\":1500}\n")),
        ("1000,false,4000\n0,true,5000\n", None),
        ("1000,false,4000\nmalformed\n", None),
    ] {
        let mut child = binary().args(["batch", "-"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        if let Some(expected) = expected {
            assert!(out.status.success());
            assert_eq!(String::from_utf8(out.stdout).unwrap(), expected);
        } else {
            assert_eq!(out.status.code(), Some(2));
            assert!(out.stdout.is_empty());
            assert!(String::from_utf8(out.stderr).unwrap().contains("line 2:"));
        }
    }
}

#[test]
fn file_input_and_file_errors_follow_the_same_boundary() {
    let path = std::env::temp_dir().join(format!("vera-parcel-input-{}", std::process::id()));
    std::fs::write(&path, "1000,false,5000\n1000,true,5000\n").unwrap();
    let out = binary().arg("batch").arg(&path).output().unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "{\"line\":1,\"shipping_cents\":0}\n{\"line\":2,\"shipping_cents\":1500}\n"
    );
    let missing = binary().arg("batch").arg(&path).output().unwrap();
    assert_eq!(missing.status.code(), Some(2));
    assert!(missing.stdout.is_empty());
    assert!(!missing.stderr.is_empty());
}
