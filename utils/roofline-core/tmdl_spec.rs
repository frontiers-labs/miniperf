use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::Write as _,
    fs,
    path::Path,
};

use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    ScalarInteger,
    ScalarFloat,
    ScalarDouble,
    VectorInteger,
    VectorFloat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Cost {
    kind: Kind,
    factor: u64,
    sew_scale: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Classification {
    Counted(Cost),
    NonCompute,
    Unclassified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Domain {
    Integer,
    Float,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpressionClass {
    Counted {
        domain: Domain,
        factor: u64,
        sew_scale: u64,
    },
    NonCompute,
    Ambiguous,
}

struct Entry {
    mnemonic: String,
    masked: bool,
    operation_name: String,
    classification: Classification,
}

pub fn generate(input: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<(), Box<dyn Error>> {
    let document: Value = serde_json::from_slice(&fs::read(input.as_ref())?)?;
    if document.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("vendored TMDL document is not AST contract version 1".into());
    }

    let mut entries = Vec::new();
    let mut operation_names = BTreeSet::new();
    let files = document
        .get("files")
        .and_then(Value::as_array)
        .ok_or("TMDL document has no files array")?;
    let items = files
        .iter()
        .filter_map(|file| file.get("items").and_then(Value::as_array))
        .flatten()
        .collect::<Vec<_>>();
    let templates = items
        .iter()
        .filter(|item| item.get("kind").and_then(Value::as_str) == Some("template"))
        .filter_map(|item| Some((item.get("name")?.as_str()?, *item)))
        .collect::<BTreeMap<_, _>>();
    for item in items
        .iter()
        .copied()
        .filter(|item| item.get("kind").and_then(Value::as_str) == Some("instruction"))
    {
        let mnemonic = string_parameter(item, "MNEMONIC")
            .or_else(|| {
                let template = templates.get(item.get("template")?.as_str()?)?;
                string_parameter(template, "MNEMONIC")
            })
            .ok_or("TMDL instruction has no string MNEMONIC parameter")?;
        let opname = string_parameter(item, "OPNAME");
        let operation_name = opname.unwrap_or(mnemonic);
        if !operation_names.insert(operation_name.to_owned()) {
            return Err(format!("duplicate TMDL operation name '{operation_name}'").into());
        }
        entries.push(Entry {
            mnemonic: mnemonic.to_owned(),
            masked: opname.is_some_and(|name| name.ends_with(".m")),
            operation_name: operation_name.to_owned(),
            classification: classify_instruction(item),
        });
    }

    // Disassembly omits TIR's variant names. Count a mnemonic only when all
    // variants agree, including explicit and dynamic rounding modes.
    let mut unique: BTreeMap<(String, bool), Entry> = BTreeMap::new();
    for entry in entries {
        let key = (entry.mnemonic.clone(), entry.masked);
        if let Some(previous) = unique.get_mut(&key) {
            if previous.classification != entry.classification {
                previous.classification = Classification::Unclassified;
            }
        } else {
            unique.insert(key, entry);
        }
    }
    let entries = unique.into_values().collect::<Vec<_>>();
    validate_keys(&entries)?;
    fs::write(output, render(&entries))?;
    Ok(())
}

fn string_parameter<'a>(item: &'a Value, name: &str) -> Option<&'a str> {
    item.get("parameters")?
        .as_array()?
        .iter()
        .find(|parameter| parameter.get("name").and_then(Value::as_str) == Some(name))?
        .get("value")?
        .get("value")?
        .as_str()
}

fn validate_keys(entries: &[Entry]) -> Result<(), Box<dyn Error>> {
    let keys = entries
        .iter()
        .map(|entry| (entry.mnemonic.as_str(), entry.masked))
        .collect::<BTreeSet<_>>();

    for entry in entries.iter().filter(|entry| entry.masked) {
        if !keys.contains(&(entry.mnemonic.as_str(), false)) {
            return Err(format!(
                "masked TMDL operation '{}' has no unmasked '{}' partner",
                entry.operation_name, entry.mnemonic
            )
            .into());
        }
    }
    Ok(())
}

fn classify_instruction(item: &Value) -> Classification {
    let Some(behavior) = item.get("behavior") else {
        return Classification::Unclassified;
    };
    if contains_builtin(behavior, "todo") {
        return Classification::Unclassified;
    }

    let isas = item
        .get("isas")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    // Packing the floating-point status fields is part of a CSR transfer,
    // not arithmetic performed by the program.
    if isas.contains(&"Zicsr") {
        return Classification::NonCompute;
    }
    let vector = isas.iter().any(|isa| matches!(*isa, "RVV" | "VF"));
    let float = isas
        .iter()
        .any(|isa| matches!(*isa, "F" | "D" | "D64" | "VF"));
    let domain = if float {
        Domain::Float
    } else {
        Domain::Integer
    };

    let expression_class = if vector {
        classify_vector_behavior(behavior, domain)
    } else {
        classify_scalar_behavior(behavior, domain)
    };

    match expression_class {
        ExpressionClass::NonCompute => Classification::NonCompute,
        ExpressionClass::Ambiguous => Classification::Unclassified,
        ExpressionClass::Counted {
            domain,
            factor,
            sew_scale,
        } => {
            let kind = match (vector, domain) {
                (true, Domain::Integer) => Kind::VectorInteger,
                (true, Domain::Float) => Kind::VectorFloat,
                (false, Domain::Integer) => Kind::ScalarInteger,
                (false, Domain::Float) if isas.contains(&"D") || isas.contains(&"D64") => {
                    Kind::ScalarDouble
                }
                (false, Domain::Float) => Kind::ScalarFloat,
            };
            Classification::Counted(Cost {
                kind,
                factor,
                sew_scale: if vector { sew_scale } else { 1 },
            })
        }
    }
}

fn classify_vector_behavior(behavior: &Value, domain: Domain) -> ExpressionClass {
    let mut classes = Vec::new();
    collect_vector_lambdas(behavior, domain, &mut classes);
    merge_classes(classes)
}

fn collect_vector_lambdas(node: &Value, domain: Domain, classes: &mut Vec<ExpressionClass>) {
    match node {
        Value::Array(values) => {
            for value in values {
                collect_vector_lambdas(value, domain, classes);
            }
        }
        Value::Object(object) => {
            if node.get("kind").and_then(Value::as_str) == Some("call")
                && matches!(builtin_name(node), Some("map" | "reduce"))
            {
                if let Some(arguments) = node.get("arguments").and_then(Value::as_array) {
                    for lambda in arguments.iter().filter(|argument| {
                        argument.get("kind").and_then(Value::as_str) == Some("lambda")
                    }) {
                        let parameters = lambda
                            .get("parameters")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect::<BTreeSet<_>>();
                        if let Some(body) = lambda.get("body") {
                            classes.push(classify_expression(body, domain, &parameters));
                        }
                    }
                }
            }
            for value in object.values() {
                collect_vector_lambdas(value, domain, classes);
            }
        }
        _ => {}
    }
}

fn classify_scalar_behavior(behavior: &Value, domain: Domain) -> ExpressionClass {
    let mut classes = Vec::new();
    let resolved = resolve_bindings(behavior, &BTreeMap::new());
    collect_scalar_results(&resolved, domain, &mut classes);
    merge_classes(classes)
}

fn resolve_bindings(node: &Value, bindings: &BTreeMap<String, Value>) -> Value {
    if node.get("kind").and_then(Value::as_str) == Some("identifier") {
        if let Some(value) = node
            .get("name")
            .and_then(Value::as_str)
            .and_then(|name| bindings.get(name))
        {
            return value.clone();
        }
    }
    if node.get("kind").and_then(Value::as_str) == Some("block") {
        let mut resolved = node.clone();
        let mut bindings = bindings.clone();
        if let Some(statements) = resolved.get_mut("statements").and_then(Value::as_array_mut) {
            for statement in statements {
                *statement = resolve_bindings(statement, &bindings);
                if statement.get("kind").and_then(Value::as_str) == Some("let") {
                    if let (Some(name), Some(value)) = (
                        statement.get("name").and_then(Value::as_str),
                        statement.get("value"),
                    ) {
                        bindings.insert(name.to_owned(), value.clone());
                    }
                }
            }
        }
        return resolved;
    }
    match node {
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| resolve_bindings(value, bindings))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), resolve_bindings(value, bindings)))
                .collect(),
        ),
        _ => node.clone(),
    }
}

fn collect_scalar_results(node: &Value, domain: Domain, classes: &mut Vec<ExpressionClass>) {
    match node {
        Value::Array(values) => {
            for value in values {
                collect_scalar_results(value, domain, classes);
            }
        }
        Value::Object(object) => {
            if node.get("kind").and_then(Value::as_str) == Some("assign") {
                let destination = node
                    .get("destination")
                    .and_then(|destination| destination.get("name"))
                    .and_then(Value::as_str);
                if matches!(destination, Some("rd" | "fd")) {
                    if let Some(value) = node.get("value") {
                        classes.push(if contains_path(value, &["PC", "pc"]) {
                            ExpressionClass::NonCompute
                        } else {
                            classify_expression(value, domain, &BTreeSet::new())
                        });
                    }
                    return;
                }
            }
            for value in object.values() {
                collect_scalar_results(value, domain, classes);
            }
        }
        _ => {}
    }
}

fn merge_classes(classes: Vec<ExpressionClass>) -> ExpressionClass {
    let mut counted = None;
    let mut ambiguous = false;
    for class in classes {
        match class {
            ExpressionClass::Counted {
                domain,
                factor,
                sew_scale,
            } => {
                let value = (domain, factor, sew_scale);
                if counted
                    .replace(value)
                    .is_some_and(|previous| previous != value)
                {
                    ambiguous = true;
                }
            }
            ExpressionClass::Ambiguous => ambiguous = true,
            ExpressionClass::NonCompute => {}
        }
    }
    if ambiguous {
        ExpressionClass::Ambiguous
    } else if let Some((domain, factor, sew_scale)) = counted {
        ExpressionClass::Counted {
            domain,
            factor,
            sew_scale,
        }
    } else {
        ExpressionClass::NonCompute
    }
}

fn classify_expression(
    expression: &Value,
    domain: Domain,
    parameters: &BTreeSet<&str>,
) -> ExpressionClass {
    match expression.get("kind").and_then(Value::as_str) {
        Some("block") => expression
            .get("statements")
            .and_then(Value::as_array)
            .and_then(|statements| statements.last())
            .map(|last| classify_expression(last, domain, parameters))
            .unwrap_or(ExpressionClass::NonCompute),
        Some("lambda") => expression
            .get("body")
            .map(|body| classify_expression(body, domain, parameters))
            .unwrap_or(ExpressionClass::Ambiguous),
        Some("call") => classify_call(expression, domain, parameters),
        Some("binary") if domain == Domain::Integer => {
            let Some(op) = expression.get("op").and_then(Value::as_str) else {
                return ExpressionClass::Ambiguous;
            };
            if !is_integer_operation(op) {
                return ExpressionClass::NonCompute;
            }
            let paired_multiply_add = (matches!(op, "add" | "subtract")
                && direct_binary_operand(expression, "multiply"))
                || (op == "multiply"
                    && (direct_binary_operand(expression, "add")
                        || direct_binary_operand(expression, "subtract")));
            let fused = paired_multiply_add
                && !contains_binary(expression, "divide")
                && !contains_binary(expression, "unsigned_divide");
            ExpressionClass::Counted {
                domain,
                factor: if fused { 2 } else { 1 },
                sew_scale: 1,
            }
        }
        Some("binary") => ExpressionClass::NonCompute,
        Some("if") => classify_if(expression, domain, parameters),
        Some("assign") => expression
            .get("value")
            .map(|value| classify_expression(value, domain, parameters))
            .unwrap_or(ExpressionClass::Ambiguous),
        Some("identifier" | "path" | "field" | "integer" | "string") => ExpressionClass::NonCompute,
        Some(_) => ExpressionClass::NonCompute,
        None => ExpressionClass::Ambiguous,
    }
}

fn classify_call(
    expression: &Value,
    domain: Domain,
    parameters: &BTreeSet<&str>,
) -> ExpressionClass {
    let Some(name) = builtin_name(expression) else {
        return ExpressionClass::NonCompute;
    };
    match name {
        "fadd" | "fsub" | "fmul" | "fdiv" | "fmin" | "fmax" | "sqrt" => {
            principal_float_operation(expression, 1)
        }
        "fma" => principal_float_operation(expression, 2),
        "concat" => expression
            .get("arguments")
            .and_then(Value::as_array)
            .and_then(|arguments| arguments.first())
            .map(|argument| classify_expression(argument, domain, parameters))
            .unwrap_or(ExpressionClass::Ambiguous),
        "map" | "reduce" => {
            let classes = expression
                .get("arguments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|argument| argument.get("kind").and_then(Value::as_str) == Some("lambda"))
                .filter_map(|lambda| lambda.get("body"))
                .map(|body| classify_expression(body, domain, parameters))
                .collect();
            merge_classes(classes)
        }
        "zext" | "sext" | "bitcast" if domain == Domain::Integer => expression
            .get("arguments")
            .and_then(Value::as_array)
            .and_then(|arguments| arguments.first())
            .map(|argument| classify_expression(argument, domain, parameters))
            .unwrap_or(ExpressionClass::Ambiguous),
        "extract" if domain == Domain::Integer => {
            let source = expression
                .get("arguments")
                .and_then(Value::as_array)
                .and_then(|arguments| arguments.first());
            if source.is_some_and(|source| contains_binary(source, "multiply")) {
                ExpressionClass::Counted {
                    domain,
                    factor: 1,
                    sew_scale: 1,
                }
            } else {
                ExpressionClass::NonCompute
            }
        }
        "atomic_rmw" if domain == Domain::Integer => {
            let operation = expression
                .get("arguments")
                .and_then(Value::as_array)
                .and_then(|arguments| arguments.first())
                .and_then(|operation| operation.get("name"))
                .and_then(Value::as_str);
            if matches!(
                operation,
                Some("add" | "and" | "or" | "xor" | "min" | "max")
            ) {
                ExpressionClass::Counted {
                    domain,
                    factor: 1,
                    sew_scale: 1,
                }
            } else {
                ExpressionClass::NonCompute
            }
        }
        "todo" => ExpressionClass::Ambiguous,
        _ => ExpressionClass::NonCompute,
    }
}

fn classify_if(expression: &Value, domain: Domain, parameters: &BTreeSet<&str>) -> ExpressionClass {
    let condition = expression.get("condition");
    let then_class = expression
        .get("then")
        .map(|branch| classify_expression(branch, domain, parameters))
        .unwrap_or(ExpressionClass::Ambiguous);
    let else_class = expression
        .get("else")
        .map(|branch| classify_expression(branch, domain, parameters))
        .unwrap_or(ExpressionClass::NonCompute);

    if condition.is_some_and(|condition| {
        condition.get("kind").and_then(Value::as_str) == Some("identifier")
            && condition
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| parameters.contains(name))
    }) {
        return then_class;
    }

    let merged = merge_classes(vec![then_class, else_class]);
    if merged != ExpressionClass::NonCompute || domain != Domain::Integer {
        return merged;
    }
    if condition.is_some_and(|condition| integer_select(condition, expression, parameters)) {
        ExpressionClass::Counted {
            domain,
            factor: 1,
            sew_scale: 1,
        }
    } else {
        ExpressionClass::NonCompute
    }
}

fn principal_float_operation(expression: &Value, factor: u64) -> ExpressionClass {
    let widening_operand = expression
        .get("arguments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(final_expression)
        .filter(|argument| builtin_name(argument) == Some("fcvt"))
        .any(|conversion| {
            conversion
                .get("arguments")
                .and_then(Value::as_array)
                .is_some_and(|arguments| arguments.iter().skip(1).any(contains_sew_times_two))
        });
    ExpressionClass::Counted {
        domain: Domain::Float,
        factor,
        sew_scale: if widening_operand { 2 } else { 1 },
    }
}

fn integer_select(condition: &Value, expression: &Value, parameters: &BTreeSet<&str>) -> bool {
    let comparison = condition.get("kind").and_then(Value::as_str) == Some("binary")
        && condition
            .get("op")
            .and_then(Value::as_str)
            .is_some_and(is_comparison);
    if !comparison {
        return false;
    }
    if parameters.is_empty() {
        return true;
    }
    ["then", "else"].iter().all(|branch| {
        let Some(branch) = expression.get(branch) else {
            return false;
        };
        let branch = final_expression(branch);
        branch.get("kind").and_then(Value::as_str) == Some("identifier")
            && branch
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| parameters.contains(name))
    })
}

fn direct_binary_operand(expression: &Value, expected: &str) -> bool {
    ["lhs", "rhs"].iter().any(|side| {
        expression.get(side).is_some_and(|operand| {
            operand.get("kind").and_then(Value::as_str) == Some("binary")
                && operand.get("op").and_then(Value::as_str) == Some(expected)
        })
    })
}

fn contains_binary(value: &Value, expected: &str) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|value| contains_binary(value, expected)),
        Value::Object(object) => {
            (value.get("kind").and_then(Value::as_str) == Some("binary")
                && value.get("op").and_then(Value::as_str) == Some(expected))
                || object
                    .values()
                    .any(|value| contains_binary(value, expected))
        }
        _ => false,
    }
}

fn final_expression(mut expression: &Value) -> &Value {
    while expression.get("kind").and_then(Value::as_str) == Some("block") {
        let Some(last) = expression
            .get("statements")
            .and_then(Value::as_array)
            .and_then(|statements| statements.last())
        else {
            break;
        };
        expression = last;
    }
    expression
}

fn builtin_name(expression: &Value) -> Option<&str> {
    expression.get("callee")?.get("name")?.as_str()
}

fn is_integer_operation(op: &str) -> bool {
    matches!(
        op,
        "add"
            | "subtract"
            | "multiply"
            | "divide"
            | "unsigned_divide"
            | "bitwise_and"
            | "bitwise_or"
            | "bitwise_xor"
            | "shift_left_logical"
            | "shift_right_logical"
            | "shift_right_arithmetic"
            | "equal"
            | "not_equal"
            | "less_than"
            | "greater_than"
            | "less_than_equal"
            | "greater_than_equal"
            | "unsigned_less_than"
            | "unsigned_greater_than"
            | "unsigned_less_than_equal"
            | "unsigned_greater_than_equal"
    )
}

fn is_comparison(op: &str) -> bool {
    matches!(
        op,
        "equal"
            | "not_equal"
            | "less_than"
            | "greater_than"
            | "less_than_equal"
            | "greater_than_equal"
            | "unsigned_less_than"
            | "unsigned_greater_than"
            | "unsigned_less_than_equal"
            | "unsigned_greater_than_equal"
    )
}

fn contains_builtin(value: &Value, expected: &str) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|value| contains_builtin(value, expected)),
        Value::Object(object) => {
            (value.get("kind").and_then(Value::as_str) == Some("call")
                && builtin_name(value) == Some(expected))
                || object
                    .values()
                    .any(|value| contains_builtin(value, expected))
        }
        _ => false,
    }
}

fn contains_path(value: &Value, expected: &[&str]) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|value| contains_path(value, expected)),
        Value::Object(object) => {
            let matches = value.get("kind").and_then(Value::as_str) == Some("path")
                && value
                    .get("segments")
                    .and_then(Value::as_array)
                    .is_some_and(|segments| {
                        segments.len() == expected.len()
                            && segments
                                .iter()
                                .zip(expected)
                                .all(|(actual, expected)| actual.as_str() == Some(expected))
                    });
            matches || object.values().any(|value| contains_path(value, expected))
        }
        _ => false,
    }
}

fn contains_sew_times_two(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(contains_sew_times_two),
        Value::Object(object) => {
            let matches = value.get("kind").and_then(Value::as_str) == Some("binary")
                && value.get("op").and_then(Value::as_str) == Some("multiply")
                && ((value.get("lhs").is_some_and(is_sew) && value.get("rhs").is_some_and(is_two))
                    || (value.get("rhs").is_some_and(is_sew)
                        && value.get("lhs").is_some_and(is_two)));
            matches || object.values().any(contains_sew_times_two)
        }
        _ => false,
    }
}

fn is_sew(value: &Value) -> bool {
    value.get("kind").and_then(Value::as_str) == Some("path")
        && value
            .get("segments")
            .and_then(Value::as_array)
            .is_some_and(|segments| {
                segments.len() == 2
                    && segments[0].as_str() == Some("VCFG")
                    && segments[1].as_str() == Some("sew")
            })
}

fn is_two(value: &Value) -> bool {
    if value.get("kind").and_then(Value::as_str) == Some("integer") {
        return value
            .get("value")
            .and_then(Value::as_str)
            .and_then(parse_integer)
            == Some(2);
    }
    value.get("kind").and_then(Value::as_str) == Some("call")
        && matches!(builtin_name(value), Some("zext" | "sext"))
        && value
            .get("arguments")
            .and_then(Value::as_array)
            .and_then(|arguments| arguments.first())
            .is_some_and(is_two)
}

fn parse_integer(value: &str) -> Option<u64> {
    if let Some(value) = value.strip_prefix("0b") {
        u64::from_str_radix(value, 2).ok()
    } else if let Some(value) = value.strip_prefix("0x") {
        u64::from_str_radix(value, 16).ok()
    } else {
        value.parse().ok()
    }
}

fn render(entries: &[Entry]) -> String {
    let mut output = String::from(
        "// Generated from spec/riscv-ast-v1.json by build.rs.\n\
         static RISCV_OPERATIONS: &[RiscvOperationSpec] = &[\n",
    );
    for entry in entries {
        writeln!(
            output,
            "    RiscvOperationSpec {{ mnemonic: {:?}, masked: {}, classification: {} }},",
            entry.mnemonic,
            entry.masked,
            render_classification(entry.classification)
        )
        .unwrap();
    }
    output.push_str("] ;\n");
    output
}

fn render_classification(classification: Classification) -> String {
    match classification {
        Classification::NonCompute => "RiscvClassification::NonCompute".to_owned(),
        Classification::Unclassified => "RiscvClassification::Unclassified".to_owned(),
        Classification::Counted(cost) => format!(
            "RiscvClassification::Counted(RiscvCost {{ kind: {}, factor: {}, sew_scale: {} }})",
            match cost.kind {
                Kind::ScalarInteger => "RiscvKind::ScalarInteger",
                Kind::ScalarFloat => "RiscvKind::ScalarFloat",
                Kind::ScalarDouble => "RiscvKind::ScalarDouble",
                Kind::VectorInteger => "RiscvKind::VectorInteger",
                Kind::VectorFloat => "RiscvKind::VectorFloat",
            },
            cost.factor,
            cost.sew_scale
        ),
    }
}
