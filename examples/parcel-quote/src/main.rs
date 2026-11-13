//! Conventional Rust I/O shell; only the policy is formally verified.
#![forbid(unsafe_code)]
#![deny(unused_must_use)]
use std::io::{self, Read};

include!(concat!(env!("OUT_DIR"), "/verified.module.rs"));

fn quote(fields: &[&str]) -> Result<u64, String> {
    if fields.len() != 3 {
        return Err("expected weight_grams,express,subtotal_cents".into());
    }
    let weight = fields[0]
        .trim()
        .parse::<u64>()
        .map_err(|_| "invalid weight")?;
    let express = fields[1]
        .trim()
        .parse::<bool>()
        .map_err(|_| "invalid express flag")?;
    let subtotal = fields[2]
        .trim()
        .parse::<u64>()
        .map_err(|_| "invalid subtotal")?;
    verified::functions::call_shipping_fee(weight, express, subtotal)
        .map_err(|error| format!("policy rejected request: {error:?}"))
}

fn execute(args: &[String]) -> Result<Vec<String>, String> {
    match args {
        [command, weight, express, subtotal] if command == "quote" => {
            let fee = quote(&[weight, express, subtotal])?;
            Ok(vec![format!("{{\"shipping_cents\":{fee}}}")])
        }
        [command, path] if command == "batch" => {
            let input = if path == "-" {
                let mut input = String::new();
                io::stdin()
                    .read_to_string(&mut input)
                    .map_err(|e| e.to_string())?;
                input
            } else {
                std::fs::read_to_string(path).map_err(|e| e.to_string())?
            };
            // Buffer output so malformed/rejected input does not emit a partial quote set.
            input
                .lines()
                .enumerate()
                .map(|(index, line)| {
                    let fee = quote(&line.split(',').collect::<Vec<_>>())
                        .map_err(|e| format!("line {}: {e}", index + 1))?;
                    Ok(format!(
                        "{{\"line\":{},\"shipping_cents\":{fee}}}",
                        index + 1
                    ))
                })
                .collect()
        }
        _ => Err(
            "usage: parcel-quote quote WEIGHT_GRAMS EXPRESS SUBTOTAL_CENTS | batch FILE|-".into(),
        ),
    }
}

fn main() {
    match execute(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}
