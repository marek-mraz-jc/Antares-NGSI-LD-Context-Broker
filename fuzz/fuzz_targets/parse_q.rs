//! 4.9 q= grammar: must never panic, whatever the bytes, and a parsed query
//! renders to text that parses back to the same query (T-3463), save the one
//! case render.rs documents: a pattern whose own text carries a `"`.
#![no_main]
use antares_ql::{CmpOp, QNode, QValue};
use libfuzzer_sys::fuzz_target;

fn pattern_with_quote(node: &QNode) -> bool {
    match node {
        QNode::And(items) | QNode::Or(items) => items.iter().any(pattern_with_quote),
        QNode::Cmp { op: CmpOp::Pattern | CmpOp::NotPattern, value: QValue::Str(s), .. } => s.contains('"'),
        _ => false,
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(node) = antares_ql::parse_q(s) {
            if pattern_with_quote(&node) {
                return;
            }
            let rendered = node.to_string();
            let again = antares_ql::parse_q(&rendered)
                .unwrap_or_else(|e| panic!("{s:?} rendered as {rendered:?}, which does not parse: {e:?}"));
            assert_eq!(again, node, "{s:?} rendered as {rendered:?}");
        }
    }
});
