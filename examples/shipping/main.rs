//! The unverified host parses arguments; the verified decision controls stdout.
mod verified;

fn quote(args: &[String]) -> Result<u64, String> {
    if args.len() != 3 {
        return Err("usage: shipping WEIGHT_GRAMS EXPRESS(true|false) SUBTOTAL_CENTS".into());
    }
    let weight = args[0].parse::<u64>().map_err(|_| "invalid weight")?;
    let express = args[1].parse::<bool>().map_err(|_| "invalid express flag")?;
    let subtotal = args[2].parse::<u64>().map_err(|_| "invalid subtotal")?;
    verified::functions::call_shipping_fee(weight, express, subtotal)
        .map_err(|error| format!("policy rejected request: {error:?}"))
}

fn main() {
    match quote(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(cents) => println!("{{\"shipping_cents\":{cents}}}"),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}
