use splicer::cviz::model::{ComponentNode, CompositionGraph, InterfaceConnection};
use splicer::preview_with_graph;

const IFACE: &str = "wasi:http/handler@0.3.0";

fn three_node_chain() -> (CompositionGraph, [String; 3]) {
    let mut graph = CompositionGraph::new();

    let mut srv = ComponentNode::new("$srv".to_string(), 0, 0);
    srv.add_import(InterfaceConnection {
        interface_name: IFACE.into(),
        source_instance: None,
        is_host_import: true,
        interface_type: None,
        fingerprint: None,
    });
    graph.add_node(1, srv);

    let mut mw_a = ComponentNode::new("$mw-a".to_string(), 1, 1);
    mw_a.add_import(InterfaceConnection {
        interface_name: IFACE.into(),
        source_instance: Some(1),
        is_host_import: false,
        interface_type: None,
        fingerprint: None,
    });
    graph.add_node(2, mw_a);

    let mut mw_b = ComponentNode::new("$mw-b".to_string(), 2, 2);
    mw_b.add_import(InterfaceConnection {
        interface_name: IFACE.into(),
        source_instance: Some(2),
        is_host_import: false,
        interface_type: None,
        fingerprint: None,
    });
    graph.add_node(3, mw_b);

    graph.add_export(IFACE.into(), 3, None);

    // chain is innermost -> outermost: [srv, mw-a, mw-b]
    let edge_srv_under_mwa = format!("{IFACE}::mw-a->srv");
    let edge_mwa_under_mwb = format!("{IFACE}::mw-b->mw-a");
    let edge_mwb_boundary = format!("{IFACE}::->mw-b");
    (
        graph,
        [edge_srv_under_mwa, edge_mwa_under_mwb, edge_mwb_boundary],
    )
}

#[test]
fn before_provider_glob_highlights_every_position() {
    let (graph, [e0, e1, e2]) = three_node_chain();
    let yaml = r#"
version: 1
rules:
  - before:
      interface: "wasi:http/handler@0.3.0"
    inject:
      - name: noop
        path: ./noop.wasm
"#;
    let out = preview_with_graph(graph, yaml, None).expect("preview ok");
    assert!(out.highlights.is_edge_highlighted(&e0));
    assert!(out.highlights.is_edge_highlighted(&e1));
    assert!(out.highlights.is_edge_highlighted(&e2));
    // One rule, tag id = 1 attached to every edge.
    assert_eq!(out.highlights.edge_tag_ids(&e0), vec![1]);
    assert_eq!(out.highlights.edge_tag_ids(&e2), vec![1]);
    assert!(out.unmatched_rules.is_empty());
}

#[test]
fn between_inner_outer_only_picks_internal_edge() {
    let (graph, [internal_low, internal_high, boundary]) = three_node_chain();
    let yaml = r#"
version: 1
rules:
  - between:
      interface: "wasi:http/handler@0.3.0"
      inner:
        name: srv
      outer:
        name: mw-a
    inject:
      - name: noop
        path: ./noop.wasm
"#;
    let out = preview_with_graph(graph, yaml, None).expect("preview ok");
    assert!(out.highlights.is_edge_highlighted(&internal_low));
    assert!(!out.highlights.is_edge_highlighted(&internal_high));
    assert!(!out.highlights.is_edge_highlighted(&boundary));
    assert!(out.unmatched_rules.is_empty());
}

#[test]
fn multiple_rules_each_get_their_own_tag() {
    let (graph, [e0, e1, _boundary]) = three_node_chain();
    let yaml = r#"
version: 1
rules:
  - between:
      interface: "wasi:http/handler@0.3.0"
      inner:
        name: srv
      outer:
        name: mw-a
    inject:
      - name: noop-a
        path: ./a.wasm
  - between:
      interface: "wasi:http/handler@0.3.0"
      inner:
        name: mw-a
      outer:
        name: mw-b
    inject:
      - name: noop-b
        path: ./b.wasm
"#;
    let out = preview_with_graph(graph, yaml, None).expect("preview ok");
    // Tag ids = 1-based rule index.
    assert_eq!(out.highlights.edge_tag_ids(&e0), vec![1]);
    assert_eq!(out.highlights.edge_tag_ids(&e1), vec![2]);
    // Legend lines include both descriptions, sorted by tag id.
    let lines = out.highlights.tag_lines();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("1 "), "got: {:?}", lines[0]);
    assert!(lines[1].starts_with("2 "), "got: {:?}", lines[1]);
}

#[test]
fn unmatched_rule_is_reported_not_errored() {
    let (graph, _) = three_node_chain();
    let yaml = r#"
version: 1
rules:
  - before:
      interface: "does:not/exist@9.9.9"
    inject:
      - name: noop
        path: ./noop.wasm
"#;
    let out = preview_with_graph(graph, yaml, None).expect("preview ok");
    assert!(out.highlights.is_empty());
    assert_eq!(out.unmatched_rules, vec![1]);
}

#[test]
fn only_rule_filter_skips_other_rules_entirely() {
    let (graph, [e0, e1, _]) = three_node_chain();
    let yaml = r#"
version: 1
rules:
  - between:
      interface: "wasi:http/handler@0.3.0"
      inner:
        name: srv
      outer:
        name: mw-a
    inject:
      - name: a
        path: ./a.wasm
  - between:
      interface: "wasi:http/handler@0.3.0"
      inner:
        name: mw-a
      outer:
        name: mw-b
    inject:
      - name: b
        path: ./b.wasm
"#;
    // Filter to rule 2 — rule 1 must not contribute highlights, nor be
    // reported as unmatched (it didn't run).
    let out = preview_with_graph(graph, yaml, Some(2)).expect("preview ok");
    assert!(!out.highlights.is_edge_highlighted(&e0));
    assert_eq!(out.highlights.edge_tag_ids(&e1), vec![2]);
    assert!(out.unmatched_rules.is_empty());
}
