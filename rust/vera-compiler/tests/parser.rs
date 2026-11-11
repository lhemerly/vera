use serde_json::{json, Value};
use vera_compiler::ast::{Decl, Expr, Stmt};
use vera_compiler::parser::{parse_source, MAX_EXPRESSION_NESTING};

fn canonical(source: &str) -> Value {
    parse_source(source).unwrap().canonical_json()
}

fn body(expression: &str) -> String {
    format!("transition go() reads [] writes [] body {{let %value:i64={expression};commit;}}")
}

#[test]
fn empty_and_optional_module_have_the_public_canonical_shape() {
    assert_eq!(
        canonical(""),
        json!({"node":"Module","name":null,"decls":[]})
    );
    assert_eq!(
        canonical("module Example; effect Event(value:u64);"),
        json!({
            "node":"Module", "name":"Example", "decls":[{
                "node":"EffectDecl", "name":"Event", "params":[{
                    "node":"Param", "name":"value", "type":{
                        "node":"TypeRef", "name":"u64", "type_args":[], "enum_tags":[]
                    }
                }]
            }]
        })
    );
}

#[test]
fn all_scalar_type_forms_and_column_attributes_are_preserved() {
    let scalar_types = [
        "bool",
        "u8",
        "u16",
        "u32",
        "u64",
        "i64",
        "dec64",
        "dec128",
        "sym32",
        "bytes32",
        "id64<Every>",
        "enum{Open,Closed}",
    ];
    let columns = scalar_types
        .iter()
        .enumerate()
        .map(|(i, ty)| format!("col c{i}:{ty};"))
        .collect::<String>();
    let ast = canonical(&format!("table Every capacity 0x10 {{key id:id64<Every>;{columns}col total:dec64 [unique,tracked_sum];}}"));
    let table = &ast["decls"][0];
    assert_eq!(table["capacity"], json!({"integer":"16"}));
    assert_eq!(table["columns"].as_array().unwrap().len(), 13);
    for (i, ty) in scalar_types.iter().enumerate().take(10) {
        assert_eq!(table["columns"][i]["type"]["name"], *ty);
    }
    assert_eq!(
        table["columns"][10]["type"]["type_args"][0]["name"],
        "Every"
    );
    assert_eq!(
        table["columns"][11]["type"]["enum_tags"],
        json!(["Open", "Closed"])
    );
    assert_eq!(table["columns"][12]["tracked_sum"], true);
    assert_eq!(table["columns"][12]["unique"], true);
}

#[test]
fn specification_operators_obey_precedence_and_left_associativity() {
    let ast = canonical("invariant ordered {a-b-c==d+e*f || !false && true;}");
    let expr = &ast["decls"][0]["expr"];
    assert_eq!(expr["op"], "or");
    assert_eq!(expr["left"]["op"], "eq");
    assert_eq!(expr["left"]["left"]["op"], "sub");
    assert_eq!(expr["left"]["left"]["left"]["op"], "sub");
    assert_eq!(expr["left"]["right"]["op"], "add");
    assert_eq!(expr["left"]["right"]["right"]["op"], "mul");
    assert_eq!(expr["right"]["op"], "and");
    assert_eq!(expr["right"]["left"]["node"], "Unary");
    assert_eq!(expr["right"]["left"]["operand"]["value"], false);
    assert_eq!(expr["right"]["right"]["value"], true);
}

#[test]
fn canonical_and_infix_specifications_produce_the_same_tree() {
    assert_eq!(
        canonical("invariant same {and(eq(add(a,mul(b,c)),d),not(false));}"),
        canonical("invariant same {(a+b*c)==d && !false;}"),
    );
    for (symbol, operation) in [
        ("==", "eq"),
        ("!=", "ne"),
        ("<", "lt"),
        ("<=", "le"),
        (">", "gt"),
        (">=", "ge"),
    ] {
        assert_eq!(
            canonical(&format!("invariant same {{a{symbol}b;}}")),
            canonical(&format!("invariant same {{{operation}(a,b);}}")),
        );
    }
}

#[test]
fn canonical_body_has_every_operator_and_rejects_infix_sugar() {
    for operation in [
        "add", "sub", "mul", "div", "rem", "eq", "ne", "lt", "le", "gt", "ge", "and", "or", "xor",
        "min", "max",
    ] {
        let ast = canonical(&body(&format!("{operation}(1,2)")));
        assert_eq!(ast["decls"][0]["body"][0]["value"]["op"], operation);
    }
    for operation in ["not", "neg"] {
        let ast = canonical(&body(&format!("{operation}(1)")));
        assert_eq!(ast["decls"][0]["body"][0]["value"]["op"], operation);
    }
    assert_eq!(
        canonical(&body("select(true,1,2)"))["decls"][0]["body"][0]["value"]["node"],
        "Select"
    );
    for expression in ["1+2", "1==2", "true && false", "T[k].value"] {
        assert!(
            parse_source(&body(expression)).is_err(),
            "body admitted {expression}"
        );
    }
}

#[test]
fn exact_literals_never_narrow_or_round_during_parsing() {
    let huge = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
    let ast = canonical(&body(
        "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
    ));
    assert_eq!(
        ast["decls"][0]["body"][0]["value"]["value"],
        json!({"integer":huge})
    );
    let digits = "112345678901234567890123456789012345678";
    let ast = canonical(&body("-1.12345678901234567890123456789012345678"));
    assert_eq!(
        ast["decls"][0]["body"][0]["value"]["value"],
        json!({"decimal":{"negative":true,"digits":digits,"scale":38}})
    );
    let ast = canonical(&body("-0.00"));
    assert_eq!(
        ast["decls"][0]["body"][0]["value"]["value"],
        json!({"decimal":{"negative":true,"digits":"0","scale":2}})
    );
    assert_eq!(
        canonical(&body("--123"))["decls"][0]["body"][0]["value"]["value"],
        json!({"integer":"123"})
    );
}

#[test]
fn allocation_fields_require_the_matching_handle_type_and_keep_source_order() {
    let ast = canonical("transition go() reads [] writes [] body {let %key:id64<T>=table_alloc(T,{right:2,left:1});commit;}");
    let allocation = &ast["decls"][0]["body"][0];
    assert_eq!(allocation["node"], "TableAlloc");
    assert_eq!(allocation["name"], "%key");
    assert_eq!(allocation["fields"][0][0], "right");
    assert_eq!(allocation["fields"][1][0], "left");
    for ty in ["u64", "id64<Other>"] {
        let source = format!("transition go() reads [] writes [] body {{let %key:{ty}=table_alloc(T,{{value:1}});commit;}}");
        let error = parse_source(&source).unwrap_err();
        assert_eq!(error.code, "D_PARSE");
        assert!(error.message.contains("allocation result"));
    }
}

#[test]
fn nested_folds_parse_bindings_mutations_and_adjacent_generic_assignment() {
    let ast = canonical(
        r#"
transition go(k:id64<T>) reads [T[*].value] writes [T[*].value]
body {
  let %result:id64<T>=fold_bounded<2>(acc:id64<T>=k,%index in [0..1]) {
    let %inner:i64=fold_bounded<3>(inside:i64=0,j in [0..2]) {
      guard(true,ERR_REJECT);yield add(inside,1);
    };
    let %allocated:id64<T>=table_alloc(T,{value:%inner});
    table_set(T,%allocated,value,%inner);emit Changed(%allocated,%inner);
    yield %allocated;
  };commit;
}"#,
    );
    let fold = &ast["decls"][0]["body"][0]["value"];
    assert_eq!(fold["node"], "Fold");
    assert_eq!(fold["n"], json!({"integer":"2"}));
    assert_eq!(fold["acc_type"]["type_args"][0]["name"], "T");
    assert_eq!(fold["index_name"], "%index");
    assert_eq!(fold["body"][0]["value"]["node"], "Fold");
    assert_eq!(fold["body"][1]["node"], "TableAlloc");
    assert_eq!(fold["body"][2]["node"], "TableSet");
    assert_eq!(fold["body"][3]["node"], "Emit");
    assert_eq!(fold["yield_expr"]["name"], "%allocated");
}

#[test]
fn table_and_snapshot_expression_forms_remain_distinct() {
    let forms = [
        ("table_get(T,k,value)", "TableGet"),
        ("table_exists(T,k)", "TableExists"),
        ("table_lookup(T,code,value)", "TableLookup"),
        ("table_exists_unique(T,code,value)", "TableExistsUnique"),
        ("old(T[k].value)", "Old"),
        ("sum(T.value)", "SumAgg"),
        ("old_sum(T.value)", "SumAgg"),
        ("key_of<T>(k)", "KeyOf"),
        ("id64<T>(k)", "KeyOf"),
    ];
    for (expression, node) in forms {
        assert_eq!(
            canonical(&body(expression))["decls"][0]["body"][0]["value"]["node"],
            node
        );
    }
    assert_eq!(
        canonical(&body("key_of<T>(k)")),
        canonical(&body("id64<T>(k)"))
    );
    assert_eq!(
        canonical(&body("old_sum(T.value)"))["decls"][0]["body"][0]["value"]["old"],
        true
    );
    assert_eq!(
        canonical("invariant read {eq(T[k].value,T.value[k]);}")["decls"][0]["expr"]["left"]
            ["node"],
        "TableAccess"
    );
}

#[test]
fn projection_slices_and_intent_guards_preserve_all_spellings() {
    let ast = canonical(
        r#"
projection View(k:id64<T>) {
  allow_if:table_exists(T,k);
  expose:[T[k].value,T.value[k],T[*].value,T.value[*],T.value];
  enable_intents:[go when (T[k].value>=0),freeze when (true)];
}"#,
    );
    let projection = &ast["decls"][0];
    let expose = projection["expose"].as_array().unwrap();
    assert_eq!(expose.len(), 5);
    assert_eq!(expose[0], expose[1]);
    assert_eq!(expose[2], expose[3]);
    assert_eq!(expose[2], expose[4]);
    assert_eq!(projection["enable_intents"][0]["condition"]["op"], "ge");
    assert_eq!(projection["enable_intents"][1]["transition"], "freeze");
}

#[test]
fn malformed_or_truncated_input_returns_a_located_diagnostic() {
    for source in [
        "module", "table T capacity 1 {", "table T capacity -1 {key id:id64<T>;}",
        "table T capacity 1 {key id:u64;}", "effect E(x:mystery);", "effect E(x:enum{});",
        "transition go() reads [] writes [] body {}", "transition go() reads [] writes [] body {commit;commit;}",
        "transition go() reads [] writes [] body {let value:i64=1;commit;}",
        "transition go() reads [] writes [] body {let %value:i64=fold_bounded<2>(acc:i64=0,i in [0..1]) {};commit;}",
        "invariant missing {eq(1,);}", "invariant missing {1<2<3;}",
        "projection V() {allow_if:true;expose:[];}",
    ] {
        let error = parse_source(source).unwrap_err();
        assert_eq!(error.code, "D_PARSE", "{source}");
        assert!(error.span.line >= 1 && error.span.column >= 1);
        assert!(error.span.start <= error.span.end && error.span.end <= source.len());
    }
}

#[test]
fn spans_cover_native_source_but_do_not_change_canonical_identity() {
    let source = "\n transition go() reads [] writes [] body {let %value:i64=-1.5;commit;}";
    let module = parse_source(source).unwrap();
    let Decl::Transition(transition) = &module.decls[0] else {
        panic!("transition missing")
    };
    let Stmt::Let(binding) = &transition.body[0] else {
        panic!("binding missing")
    };
    let Expr::Literal(literal) = &binding.value else {
        panic!("literal missing")
    };
    assert_eq!(&source[literal.span.start..literal.span.end], "-1.5");
    assert_eq!(transition.span.line, 2);
    assert_eq!(
        canonical(source),
        canonical("transition go() reads [] writes [] body {let %value:i64=-1.5;commit;}")
    );
}

#[test]
fn nesting_limits_reject_deep_canonical_and_infix_trees_without_overflow() {
    let count = MAX_EXPRESSION_NESTING * 4;
    let canonical = format!("{}1{}", "neg(".repeat(count), ")".repeat(count));
    let groups = format!(
        "invariant deep {{{}true{};}}",
        "(".repeat(count),
        ")".repeat(count)
    );
    let flat = format!(
        "invariant deep {{{};}}",
        std::iter::repeat_n("1", count)
            .collect::<Vec<_>>()
            .join("+")
    );
    for source in [body(&canonical), groups, flat] {
        let error = parse_source(&source).unwrap_err();
        assert_eq!(error.code, "D_PARSE");
        assert!(error.message.contains("nesting"));
    }
}

#[test]
fn grammar_boundaries_do_not_silently_expand_legacy_syntax() {
    // The existing postcondition parser treats a leading snapshot accessor as
    // a complete expression; wrapping it in a comparison is the admitted form.
    assert!(parse_source(
        "transition go() reads [] writes [] ensure bad:sum(T.value)==0;body {commit;}"
    )
    .is_err());
    assert!(parse_source(
        "transition go() reads [] writes [] ensure good:eq(sum(T.value),0);body {commit;}"
    )
    .is_ok());
    assert!(parse_source("invariant bad {eq(T.value[*],0);}").is_err());
    assert!(
        parse_source("projection V() {allow_if:true;expose:[T.value[*]];enable_intents:[];}")
            .is_ok()
    );
}
